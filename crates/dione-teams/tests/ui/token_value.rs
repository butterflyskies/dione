use dione_teams::edge::BotAccessToken;
fn main() {
    let _ = BotAccessToken {value:secrecy::SecretString::from("synthetic".to_owned()),expires_in:std::time::Duration::from_secs(1)};
}
