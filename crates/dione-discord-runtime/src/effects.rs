//! Concrete SDK operations shared by gateway and application composition.
use serde_json::json;
use serenity::model::id::UserId;

/// Opens the SDK DM channel; callers retain their own authorization checks.
pub async fn create_dm_channel(
    http: &serenity::http::Http,
    user_id: UserId,
) -> Result<serenity::model::channel::PrivateChannel, Box<serenity::Error>> {
    let dm_body = json!({ "recipient_id": user_id.get().to_string() });
    http.create_private_channel(&dm_body)
        .await
        .map_err(Box::new)
}
