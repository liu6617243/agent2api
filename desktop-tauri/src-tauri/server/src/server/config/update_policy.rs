use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::{update, CONFIG, KEY_UPDATE_BETA};

pub const KEY_UPDATE_STABLE: &str = "updateStable";

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase")]
pub struct UpdateChannels {
    pub stable: bool,
    pub beta: bool,
}

impl Default for UpdateChannels {
    fn default() -> Self {
        Self {
            stable: true,
            beta: false,
        }
    }
}

impl UpdateChannels {
    pub(super) fn from_raw(raw: &Map<String, Value>) -> Self {
        let mut channels = Self {
            stable: raw
                .get(KEY_UPDATE_STABLE)
                .and_then(Value::as_bool)
                .unwrap_or(true),
            beta: raw
                .get(KEY_UPDATE_BETA)
                .and_then(Value::as_bool)
                .unwrap_or(false),
        };
        if !channels.stable && !channels.beta {
            channels.stable = true;
        }
        channels
    }

    pub fn accepts(self, prerelease: bool) -> bool {
        if prerelease {
            self.beta
        } else {
            self.stable
        }
    }

    pub fn key(self) -> &'static str {
        match (self.stable, self.beta) {
            (true, true) => "both",
            (false, true) => "beta",
            _ => "stable",
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdateChannelsPatch {
    pub stable: Option<bool>,
    pub beta: Option<bool>,
}

pub fn update_channels() -> UpdateChannels {
    if let Ok(guard) = CONFIG.read() {
        if let Some(config) = guard.as_ref() {
            return config.update_channels;
        }
    }
    UpdateChannels::default()
}

pub fn update_beta() -> bool {
    update_channels().beta
}

pub fn set_update_channels(
    patch: UpdateChannelsPatch,
) -> Result<(UpdateChannels, bool, bool), String> {
    if patch.stable.is_none() && patch.beta.is_none() {
        return Err("请提供正式版或 beta 版关注设置".to_string());
    }
    let previous = update_channels();
    let next = UpdateChannels {
        stable: patch.stable.unwrap_or(previous.stable),
        beta: patch.beta.unwrap_or(previous.beta),
    };
    if !next.stable && !next.beta {
        return Err("至少需要关注正式版或 beta 版中的一个通道".to_string());
    }
    let saved = update(|config| {
        if let Some(stable) = patch.stable {
            config
                .raw
                .insert(KEY_UPDATE_STABLE.to_string(), Value::Bool(stable));
        }
        if let Some(beta) = patch.beta {
            config
                .raw
                .insert(KEY_UPDATE_BETA.to_string(), Value::Bool(beta));
        }
        config.update_channels = next;
    });
    Ok((next, saved, next != previous))
}
