use dione_config::Config;
use dione_config::load_config_readonly;
use serde_json::{Value, json};

fn bell_diagnostics() -> Vec<Value> {
    let cases = [
        "max_semantic_distance=nan",
        "max_semantic_distance=2.1",
        "max_bells=0",
        "max_bells=101",
        "deadline_ms=0",
        "deadline_ms=2001",
        "max_bells=0\ndeadline_ms=0",
        "deadline_ms=0\nmax_bells=0",
        "enabled=true\nmax_bells=0",
        "max_bells=0\nenabled=true",
        "max_bells=0\n[[bell_rings.providers]]\nurl='http://memory/mcp'\nscope='syne'\nalias=' '",
        "max_bells=0\n[bell_rings.provider]\nurl='file:///tmp/memory'\nscope='syne'",
        "max_bells=0\n[bell_rings.provider]\nurl='http://memory/mcp'\nscope='all'",
        "max_bells='wrong'",
        "deadline_ms=[]",
        "max_semantic_distance=true",
    ];
    cases
        .into_iter()
        .map(|case| {
            let source = format!("[bell_rings]\n{case}\n");
            let error = toml::from_str::<Config>(&source).unwrap_err();
            let deserializer = toml::Deserializer::parse(&source).unwrap();
            let path_error =
                serde_path_to_error::deserialize::<_, Config>(deserializer).unwrap_err();
            let temp = tempfile::tempdir().unwrap();
            let file = temp.path().join("config.toml");
            std::fs::write(&file, &source).unwrap();
            let loaded =
                load_config_readonly(camino::Utf8Path::from_path(&file).unwrap()).unwrap_err();
            json!({"source":source,"display":error.to_string(),"path":path_error.path().to_string(),
            "path_display":path_error.inner().to_string(),"load_display":loaded.to_string()})
        })
        .collect()
}

#[test]
fn bell_config_diagnostics_match_before_extraction() {
    let actual = bell_diagnostics();
    let expected: Vec<Value> =
        serde_json::from_str(include_str!("fixtures/bell-config-diagnostics.json")).unwrap();
    assert_eq!(actual, expected);
}
