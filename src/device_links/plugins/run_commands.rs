use serde_json::{Map, Value};

use crate::device_links::config::Config;
use crate::device_links::packet::{NetworkPacket, PACKET_TYPE_RUNCOMMAND};

pub fn command_list_packet(config: &Config) -> NetworkPacket {
    let mut commands = Map::new();
    for (key, command) in config.commands() {
        if command.enabled {
            commands.insert(
                key.clone(),
                Value::Object(Map::from_iter([(
                    "name".to_string(),
                    Value::String(command.name.clone()),
                )])),
            );
        }
    }
    let mut packet = NetworkPacket::new(PACKET_TYPE_RUNCOMMAND);
    packet.set("commandList", Value::Object(commands));
    packet.set("canAddCommand", false);
    packet
}
