use anyhow::Result;
use base64::{engine::general_purpose, Engine as _};
use futures_util::StreamExt;
use openaction::*;
use reqwest::Client;
use std::collections::HashMap;
use tokio::sync::Mutex;
use zbus::fdo::DBusProxy;
use zbus::{Connection, MatchRule, MessageStream, MessageType, Proxy};
use zvariant::Value;

async fn fetch_and_convert_to_data_url(url: &str) -> Result<String> {
	let client = Client::new();
	let response = client.get(url).send().await?;
	let bytes = response.bytes().await?;
	let mime_type = infer::get(&bytes)
		.map(|info| info.mime_type())
		.unwrap_or("application/octet-stream");
	let base64_data = general_purpose::STANDARD.encode(&bytes);
	Ok(format!("data:{};base64,{}", mime_type, base64_data))
}

async fn find_active_player(conn: &Connection) -> Result<String> {
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
		.ok_or_else(|| anyhow::anyhow!("No MPRIS players found"))
}

async fn get_mpris_proxy() -> Result<Proxy<'static>> {
	let conn = Connection::session().await?;
	let player_name = find_active_player(&conn).await?;

	let proxy = Proxy::new(
		&conn,
		player_name,
		"/org/mpris/MediaPlayer2",
		"org.mpris.MediaPlayer2.Player",
	)
	.await?;

	Ok(proxy)
}

async fn call_mpris_method(method: &str) -> Result<()> {
	let proxy = get_mpris_proxy().await?;
	proxy.call_method(method, &()).await?;
	Ok(())
}

async fn cycle_repeat_mode() -> Result<()> {
	let proxy = get_mpris_proxy().await?;
	let current: String = proxy.get_property("LoopStatus").await?;
	let next = match current.as_str() {
		"None" => "Playlist",
		"Playlist" => "Track",
		"Track" => "None",
		_ => "None",
	};
	proxy.set_property("LoopStatus", next).await?;
	Ok(())
}

async fn toggle_shuffle() -> Result<()> {
	let proxy = get_mpris_proxy().await?;
	let shuffle: bool = proxy.get_property("Shuffle").await?;
	proxy.set_property("Shuffle", &(!shuffle)).await?;
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
		outbound: &mut OutboundEventManager,
	) -> EventHandlerResult {
		ACTIONS
			.lock()
			.await
			.push((event.action.clone(), event.context.clone()));

		match event.action.as_str() {
			"me.amankhanna.oampris.playpause" => {
				let proxy = get_mpris_proxy().await?;
				let metadata_value: Value = proxy.get_property("Metadata").await?;
				if let Some(art_url) = metadata_value
					.downcast_ref::<zvariant::Dict>()
					.and_then(|dict| dict.get::<str, str>("mpris:artUrl").ok().flatten())
					.map(|url| url.to_string())
				{
					let image_data_url = if art_url.starts_with("data:") {
						art_url
					} else {
						fetch_and_convert_to_data_url(&art_url).await?
					};
					outbound
						.set_image(event.context, Some(image_data_url), None)
						.await?;
				}
			}
			"me.amankhanna.oampris.repeat" => {
				let proxy = get_mpris_proxy().await?;
				let loop_status_value: Value = proxy.get_property("LoopStatus").await?;
				let loop_status = loop_status_value.downcast_ref::<str>().unwrap_or("None");
				let state = match loop_status {
					"None" => 0,
					"Playlist" => 1,
					"Track" => 2,
					_ => 0,
				};
				outbound.set_state(event.context, state).await?;
			}
			"me.amankhanna.oampris.shuffle" => {
				let proxy = get_mpris_proxy().await?;
				let shuffle_value: Value = proxy.get_property("Shuffle").await?;
				let shuffle = *shuffle_value.downcast_ref::<bool>().unwrap_or(&false);
				outbound.set_state(event.context, shuffle as u16).await?;
			}
			_ => {}
		}

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
		if let Err(e) = match event.action.as_str() {
			"me.amankhanna.oampris.playpause" => call_mpris_method("PlayPause").await,
			"me.amankhanna.oampris.stop" => call_mpris_method("Stop").await,
			"me.amankhanna.oampris.previous" => call_mpris_method("Previous").await,
			"me.amankhanna.oampris.next" => call_mpris_method("Next").await,
			"me.amankhanna.oampris.repeat" => cycle_repeat_mode().await,
			"me.amankhanna.oampris.shuffle" => toggle_shuffle().await,
			_ => return Ok(()),
		} {
			log::error!("MPRIS call failed ({}): {}", event.action, e);
		}

		Ok(())
	}
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
				if let Some(art_url) = metadata_value
					.downcast_ref::<zvariant::Dict>()
					.and_then(|dict| dict.get::<str, str>("mpris:artUrl").ok().flatten())
					.map(|url| url.to_string())
				{
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

			if let Some(loop_status_value) = changed_properties.get("LoopStatus") {
				let loop_status = loop_status_value.downcast_ref::<str>().unwrap_or("None");
				let state = match loop_status {
					"None" => 0,
					"Playlist" => 1,
					"Track" => 2,
					_ => 0,
				};
				let actions = ACTIONS.lock().await.clone();
				let mut outbound = OUTBOUND_EVENT_MANAGER.lock().await;
				if let Some(manager) = outbound.as_mut() {
					for (_, context) in actions
						.iter()
						.filter(|(uuid, _)| uuid == "me.amankhanna.oampris.repeat")
					{
						if let Err(e) = manager.set_state(context.clone(), state).await {
							log::error!("Failed to set state: {}", e);
						}
					}
				}
			}

			if let Some(shuffle_value) = changed_properties.get("Shuffle") {
				let shuffle = *shuffle_value.downcast_ref::<bool>().unwrap_or(&false);
				let actions = ACTIONS.lock().await.clone();
				let mut outbound = OUTBOUND_EVENT_MANAGER.lock().await;
				if let Some(manager) = outbound.as_mut() {
					for (_, context) in actions
						.iter()
						.filter(|(uuid, _)| uuid == "me.amankhanna.oampris.shuffle")
					{
						if let Err(e) = manager.set_state(context.clone(), shuffle as u16).await {
							log::error!("Failed to set state: {}", e);
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
