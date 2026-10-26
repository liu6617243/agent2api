use serde_json::{json, Value};

pub fn snapshot() -> Value {
    let host = super::current().map(|host| {
        let status = host.status();
        json!({
            "running": status.running, "providers": status.providers, "crashes": status.crashes,
            "disabled": status.disabled, "lastError": status.last_error,
        })
    });
    json!({
        "packs": super::list_packs(), "host": host,
        "adminPanels": super::admin_panels(), "accountFeatures": super::account_features(),
    })
}
