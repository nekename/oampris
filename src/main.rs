use base64::{engine::general_purpose, Engine as _};
use futures_util::StreamExt;
use openaction::*;
use reqwest::Client;
use std::collections::HashMap;
use tokio::sync::Mutex;
use zbus::fdo::DBusProxy;
use zbus::{Connection, MatchRule, MessageStream, MessageType, Proxy};
use zvariant::Value;

async fn find_active_player(conn: &Connection) -> Result<String, Box<dyn std::error::Error>> {
	let proxy = Proxy::new(
		conn,
		"org.freedesktop.DBus",
		"/org/freedesktop/DBus",
		"org.freedesktop.DBus",
	)
	.await?;

	let names: Vec<String> = proxy.call("ListNames", &()).await?;
	names
		.into_iter()
		.find(|name| name.starts_with("org.mpris.MediaPlayer2."))
		.ok_or_else(|| "No MPRIS players found".into())
}

async fn call_mpris_method(method: &str) -> Result<(), Box<dyn std::error::Error>> {
	let conn = Connection::session().await?;
	let player_name = find_active_player(&conn).await?;

	let proxy = Proxy::new(
		&conn,
		player_name.as_str(),
		"/org/mpris/MediaPlayer2",
		"org.mpris.MediaPlayer2.Player",
	)
	.await?;

	proxy.call_method(method, &()).await?;
	Ok(())
}

static ACTIONS: Mutex<Vec<(String, String)>> = Mutex::const_new(vec![]);

struct GlobalEventHandler {}
impl openaction::GlobalEventHandler for GlobalEventHandler {}

struct ActionEventHandler {}
impl openaction::ActionEventHandler for ActionEventHandler {
	async fn will_appear(
		&self,
		event: AppearEvent,
		_outbound: &mut OutboundEventManager,
	) -> EventHandlerResult {
		ACTIONS.lock().await.push((event.action, event.context));
		Ok(())
	}

	async fn will_disappear(
		&self,
		event: AppearEvent,
		_outbound: &mut OutboundEventManager,
	) -> EventHandlerResult {
		ACTIONS.lock().await.retain(|(_, c)| c != &event.context);
		Ok(())
	}

	async fn key_up(
		&self,
		event: KeyEvent,
		_outbound: &mut OutboundEventManager,
	) -> EventHandlerResult {
		let method = match event.action.as_str() {
			"me.amankhanna.oampris.playpause" => "PlayPause",
			"me.amankhanna.oampris.stop" => "Stop",
			"me.amankhanna.oampris.previous" => "Previous",
			"me.amankhanna.oampris.next" => "Next",
			_ => return Ok(()),
		};

		if let Err(e) = call_mpris_method(method).await {
			log::error!("MPRIS call failed ({}): {}", method, e);
		}

		Ok(())
	}
}

fn extract_art_url_from_value(value: &Value) -> Option<String> {
	value
		.downcast_ref::<zvariant::Dict>()
		.and_then(|dict| dict.get::<str, str>("mpris:artUrl").ok().flatten())
		.map(|url| url.to_string())
}

async fn fetch_and_convert_to_data_url(url: &str) -> Result<String, Box<dyn std::error::Error>> {
	let client = Client::new();
	let response = client.get(url).send().await?;
	let bytes = response.bytes().await?;
	let mime_type = infer::get(&bytes)
		.map(|info| info.mime_type())
		.unwrap_or("application/octet-stream");
	let base64_data = general_purpose::STANDARD.encode(&bytes);
	Ok(format!("data:{};base64,{}", mime_type, base64_data))
}

async fn watch_album_art() {
	let connection = match Connection::session().await {
		Ok(conn) => conn,
		Err(e) => {
			log::error!("Failed to connect to D-Bus session: {}", e);
			return;
		}
	};

	loop {
		let player_name = match find_active_player(&connection).await {
			Ok(name) => name,
			Err(_) => continue,
		};

		let signal_rule = match MatchRule::builder()
			.msg_type(MessageType::Signal)
			.interface("org.freedesktop.DBus.Properties")
			.and_then(|b| b.member("PropertiesChanged"))
			.and_then(|b| b.path("/org/mpris/MediaPlayer2"))
			.and_then(|b| b.sender(player_name.as_str()))
			.map(|b| b.build())
		{
			Ok(rule) => rule,
			Err(e) => {
				log::error!("Failed to build match rule: {}", e);
				continue;
			}
		};

		let dbus_proxy = match DBusProxy::new(&connection).await {
			Ok(proxy) => proxy,
			Err(e) => {
				log::error!("Failed to create DBus proxy: {}", e);
				continue;
			}
		};

		if let Err(e) = dbus_proxy.add_match_rule(signal_rule).await {
			log::error!("Failed to add match rule: {}", e);
			continue;
		}

		let mut stream = MessageStream::from(&connection);
		let mut last_url: Option<String> = None;

		while let Some(msg_result) = stream.next().await {
			let msg = match msg_result {
				Ok(m) => m,
				Err(e) => {
					log::error!("Error receiving message: {}", e);
					continue;
				}
			};

			let header = match msg.header() {
				Ok(h) => h,
				Err(e) => {
					log::error!("Failed to get message header: {}", e);
					continue;
				}
			};

			match header.interface().transpose() {
				Some(Ok(i)) if i.as_str() == "org.freedesktop.DBus.Properties" => {}
				_ => continue,
			};

			let body: Result<(String, HashMap<String, Value>, Vec<String>), _> = msg.body();
			let (interface, changed_properties, _) = match body {
				Ok(b) => b,
				Err(e) => {
					log::error!("Error reading message body: {}", e);
					continue;
				}
			};

			if interface != "org.mpris.MediaPlayer2.Player" {
				continue;
			}

			if let Some(metadata_value) = changed_properties.get("Metadata") {
				if let Some(art_url) = extract_art_url_from_value(metadata_value) {
					if Some(&art_url) != last_url.as_ref() {
						last_url = Some(art_url.clone());

						let image_data_url = if art_url.starts_with("data:") {
							art_url.clone()
						} else {
							match fetch_and_convert_to_data_url(&art_url).await {
								Ok(data_url) => data_url,
								Err(e) => {
									log::error!("Failed to fetch and convert image: {}", e);
									continue;
								}
							}
						};

						let actions = ACTIONS.lock().await.clone();
						let mut outbound = OUTBOUND_EVENT_MANAGER.lock().await;
						if let Some(manager) = outbound.as_mut() {
							for (_, context) in actions
								.iter()
								.filter(|(uuid, _)| uuid == "me.amankhanna.oampris.playpause")
							{
								if let Err(e) = manager
									.set_image(context.clone(), Some(image_data_url.clone()), None)
									.await
								{
									log::error!("Failed to set image: {}", e);
								}
							}
						}
					}
				}
			}
		}

		tokio::time::sleep(std::time::Duration::from_secs(1)).await;
	}
}

#[tokio::main]
async fn main() {
	if let Err(e) = simplelog::TermLogger::init(
		simplelog::LevelFilter::Debug,
		simplelog::Config::default(),
		simplelog::TerminalMode::Stdout,
		simplelog::ColorChoice::Never,
	) {
		eprintln!("Logger initialization failed: {}", e);
	}

	tokio::spawn(watch_album_art());

	if let Err(error) = init_plugin(GlobalEventHandler {}, ActionEventHandler {}).await {
		log::error!("Failed to initialise plugin: {}", error);
	}
}
