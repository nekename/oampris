use anyhow::Result;
use base64::{engine::general_purpose, Engine as _};
use futures_util::StreamExt;
use openaction::*;
use std::collections::HashMap;
use tokio::sync::Mutex;
use zbus::fdo::DBusProxy;
use zbus::{Connection, MatchRule, MessageStream, MessageType, Proxy};
use zvariant::Value;

async fn fetch_and_convert_to_data_url(url: &str) -> Result<String> {
	let bytes = if url.starts_with("data:") {
		return Ok(url.to_owned());
	} else if url.starts_with("file:") {
		let path = url.trim_start_matches("file://");
		std::fs::read(path)?
	} else {
		let response = reqwest::get(url).await?;
		response.bytes().await?.to_vec()
	};

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

async fn get_album_art(metadata: Option<&Value<'_>>) -> Option<String> {
	fetch_and_convert_to_data_url(
		&metadata?
			.downcast_ref::<zvariant::Dict>()
			.and_then(|dict| dict.get::<str, str>("mpris:artUrl").ok().flatten())
			.map(|url| url.to_string())?,
	)
	.await
	.ok()
}

async fn update_play_pause(
	context: String,
	outbound: &mut OutboundEventManager,
	image: Option<String>,
) -> Result<()> {
	outbound.set_image(context.clone(), image, None).await?;
	Ok(())
}

async fn update_repeat(
	context: String,
	outbound: &mut OutboundEventManager,
	loop_status: Option<&Value<'_>>,
) -> Result<()> {
	let state = match loop_status {
		Some(loop_status_value) => {
			match loop_status_value.downcast_ref::<str>().unwrap_or("None") {
				"None" => 0,
				"Playlist" => 1,
				"Track" => 2,
				_ => 0,
			}
		}
		_ => 0,
	};
	outbound.set_state(context, state).await?;
	Ok(())
}

async fn update_shuffle(
	context: String,
	outbound: &mut OutboundEventManager,
	shuffle: Option<&Value<'_>>,
) -> Result<()> {
	let state = shuffle
		.and_then(|shuffle_value| shuffle_value.downcast_ref::<bool>().copied())
		.unwrap_or(false);
	outbound.set_state(context, state as u16).await?;
	Ok(())
}

async fn update_all(outbound: &mut OutboundEventManager) {
	let proxy_result = get_mpris_proxy().await;
	let get_property = async |property: &str| match &proxy_result {
		Ok(proxy) => proxy.get_property(property).await.ok(),
		Err(_) => None,
	};
	let actions = ACTIONS.lock().await.clone();
	for (uuid, context) in actions {
		if let Err(error) = match uuid.as_str() {
			"me.amankhanna.oampris.playpause" => {
				update_play_pause(
					context,
					outbound,
					get_album_art(get_property("Metadata").await.as_ref()).await,
				)
				.await
			}
			"me.amankhanna.oampris.repeat" => {
				update_repeat(context, outbound, get_property("LoopStatus").await.as_ref()).await
			}
			"me.amankhanna.oampris.shuffle" => {
				update_shuffle(context, outbound, get_property("Shuffle").await.as_ref()).await
			}
			_ => Ok(()),
		} {
			log::error!(
				"Failed to update {}: {}",
				uuid.trim_start_matches("me.amankhanna.oampris."),
				error
			);
		}
	}
}

struct GlobalEventHandler {}
impl openaction::GlobalEventHandler for GlobalEventHandler {}

struct ActionEventHandler {}
impl openaction::ActionEventHandler for ActionEventHandler {
	async fn will_appear(
		&self,
		event: AppearEvent,
		outbound: &mut OutboundEventManager,
	) -> EventHandlerResult {
		ACTIONS.lock().await.push((event.action, event.context));
		update_all(outbound).await;
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
		if let Err(error) = match event.action.as_str() {
			"me.amankhanna.oampris.playpause" => call_mpris_method("PlayPause").await,
			"me.amankhanna.oampris.stop" => call_mpris_method("Stop").await,
			"me.amankhanna.oampris.previous" => call_mpris_method("Previous").await,
			"me.amankhanna.oampris.next" => call_mpris_method("Next").await,
			"me.amankhanna.oampris.repeat" => cycle_repeat_mode().await,
			"me.amankhanna.oampris.shuffle" => toggle_shuffle().await,
			_ => return Ok(()),
		} {
			log::error!(
				"Failed to make MPRIS call for {}: {}",
				event.action.trim_start_matches("me.amankhanna.oampris."),
				error
			);
		}

		Ok(())
	}
}

async fn watch_album_art() {
	let connection = match Connection::session().await {
		Ok(conn) => conn,
		Err(error) => {
			log::error!("Failed to connect to DBus session: {}", error);
			return;
		}
	};

	loop {
		let mut outbound = OUTBOUND_EVENT_MANAGER.lock().await;
		if let Some(manager) = outbound.as_mut() {
			let _ = update_all(manager).await;
		}
		drop(outbound);

		let player_name = match find_active_player(&connection).await {
			Ok(name) => name,
			Err(_) => {
				tokio::time::sleep(std::time::Duration::from_secs(1)).await;
				continue;
			}
		};

		let dbus_proxy = match DBusProxy::new(&connection).await {
			Ok(proxy) => proxy,
			Err(error) => {
				log::error!("Failed to create DBus proxy: {}", error);
				return;
			}
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
			Err(error) => {
				log::error!("Failed to build match rule: {}", error);
				continue;
			}
		};

		if let Err(error) = dbus_proxy.add_match_rule(signal_rule).await {
			log::error!("Failed to add match rule: {}", error);
			continue;
		}

		let name_owner_rule = MatchRule::builder()
			.msg_type(MessageType::Signal)
			.interface("org.freedesktop.DBus")
			.and_then(|b| b.member("NameOwnerChanged"))
			.map(|b| b.build());

		if let Ok(rule) = name_owner_rule {
			let _ = dbus_proxy.add_match_rule(rule).await;
		}

		let mut stream = MessageStream::from(&connection);

		while let Some(msg_result) = stream.next().await {
			let msg = match msg_result {
				Ok(m) => m,
				Err(error) => {
					log::error!("Error receiving message: {}", error);
					continue;
				}
			};

			let header = match msg.header() {
				Ok(h) => h,
				Err(error) => {
					log::error!("Failed to get message header: {}", error);
					continue;
				}
			};

			let member = header.member().ok().flatten().map(|m| m.to_string());
			if member.as_deref() == Some("NameOwnerChanged") {
				if let Ok((name, _old_owner, new_owner)) = msg.body::<(String, String, String)>() {
					if name == player_name && new_owner.is_empty() {
						break;
					}
				}
				continue;
			} else if member.as_deref() != Some("PropertiesChanged") {
				continue;
			}

			let body: Result<(String, HashMap<String, Value>, Vec<String>), _> = msg.body();
			let (interface, changed_properties, _) = match body {
				Ok(b) => b,
				Err(error) => {
					log::error!("Error reading message body: {}", error);
					continue;
				}
			};

			if interface != "org.mpris.MediaPlayer2.Player" {
				continue;
			}

			let actions = ACTIONS.lock().await.clone();
			let mut outbound = OUTBOUND_EVENT_MANAGER.lock().await;
			let Some(outbound) = outbound.as_mut() else {
				continue;
			};

			if let Some(playback_status_value) = changed_properties.get("PlaybackStatus") {
				if playback_status_value.downcast_ref::<str>() == Some("Stopped") {
					update_all(outbound).await;
					continue;
				}
			}

			let album_art_url = get_album_art(changed_properties.get("Metadata")).await;

			for (uuid, context) in actions {
				if let Err(error) = match uuid.as_str() {
					"me.amankhanna.oampris.playpause"
						if changed_properties.contains_key("Metadata") =>
					{
						update_play_pause(context.clone(), outbound, album_art_url.clone()).await
					}
					"me.amankhanna.oampris.repeat"
						if changed_properties.contains_key("LoopStatus") =>
					{
						update_repeat(
							context.clone(),
							outbound,
							changed_properties.get("LoopStatus"),
						)
						.await
					}
					"me.amankhanna.oampris.shuffle"
						if changed_properties.contains_key("Shuffle") =>
					{
						update_shuffle(context.clone(), outbound, changed_properties.get("Shuffle"))
							.await
					}
					_ => continue,
				} {
					log::error!(
						"Failed to update {} at {}: {}",
						uuid.trim_start_matches("me.amankhanna.oampris."),
						context,
						error
					);
				}
			}
		}
	}
}

#[tokio::main]
async fn main() {
	if let Err(error) = simplelog::TermLogger::init(
		simplelog::LevelFilter::Debug,
		simplelog::Config::default(),
		simplelog::TerminalMode::Stdout,
		simplelog::ColorChoice::Never,
	) {
		eprintln!("Logger initialization failed: {}", error);
	}

	tokio::spawn(watch_album_art());

	if let Err(error) = init_plugin(GlobalEventHandler {}, ActionEventHandler {}).await {
		log::error!("Failed to initialise plugin: {}", error);
	}
}
