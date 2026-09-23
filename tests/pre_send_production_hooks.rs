//! Integration tests for the production pre-send hook roster.
//!
//! These assert against [`dione::pre_send::production_pipeline`] — the same
//! expression `main` calls — rather than constructing a hook set locally, which
//! would certify a parallel wiring instead of the shipped one.
//!
//! RESIDUAL, confirmed by mutation: nothing here observes `main` itself. Editing
//! `main.rs` alone to install an empty pipeline leaves all of these green. What
//! they prove is that `production_pipeline()` contains and executes the hook,
//! not that `main` calls it.

use dione::pre_send::{
    ChannelType, ConstructId, HookDecision, HookName, NoRly, OutboundDestination,
    production_pipeline,
};
use serenity::model::id::ChannelId;

const STATUS_LINT: &str = "status-packet-lint";

fn context(text: &str) -> dione::pre_send::HookContext {
    dione::pre_send::HookContext::new(
        text,
        OutboundDestination::Channel(ChannelId::new(1)),
        ChannelType::Public,
        ConstructId::default(),
    )
}

#[test]
fn the_configured_pipeline_executes_the_status_lint() {
    let pipeline = production_pipeline().expect("pipeline builds");
    let outcome = pipeline
        .run(
            &context("\u{26A0}\u{FE0F} blocked on #375"),
            &NoRly::default(),
        )
        .expect("pipeline runs");

    let attributed: Vec<&str> = outcome
        .to_audit()
        .as_slice()
        .iter()
        .filter_map(|assessment| assessment.hook_name())
        .map(|name| name.as_str())
        .collect();

    assert!(
        attributed.contains(&STATUS_LINT),
        "no assessment attributed to the status lint; got {attributed:?}"
    );
}

#[test]
fn observe_mode_carries_findings_on_the_audit_trail_only() {
    // Pins a pre-existing asymmetry rather than assuming it: `run` appends
    // `ConstructFeedback` only under Enforce, and `observe_pipeline` hardcodes
    // Observe. A hook reporting solely through the construct stream would
    // compute findings and surface none. If this test starts failing because
    // the construct stream is populated in Observe, that is a deliberate
    // change to `pre_send` and this assertion should be revisited, not deleted.
    let pipeline = production_pipeline().expect("pipeline builds");
    let outcome = pipeline
        .run(
            &context("\u{26A0}\u{FE0F} blocked on #375"),
            &NoRly::default(),
        )
        .expect("pipeline runs");

    assert!(
        outcome.to_construct().as_slice().is_empty(),
        "Observe mode unexpectedly routed construct feedback"
    );
    assert!(
        !outcome.to_audit().as_slice().is_empty(),
        "Observe mode dropped the audit trail too — the hook reports nowhere"
    );
}

#[test]
fn the_pipeline_never_halts_or_rewrites_on_a_status_finding() {
    let dirty = "\u{26A0}\u{FE0F} blocked on #375";
    let pipeline = production_pipeline().expect("pipeline builds");
    let outcome = pipeline
        .run(&context(dirty), &NoRly::default())
        .expect("pipeline runs");

    assert_eq!(*outcome.decision(), HookDecision::Continue);
    // Byte-identical passthrough on a message that produces findings: the
    // lint observes and never edits or suppresses what gets sent.
    assert_eq!(outcome.final_text(), Some(dirty));
}

#[test]
fn a_clean_packet_produces_no_attributed_assessments() {
    let pipeline = production_pipeline().expect("pipeline builds");
    let outcome = pipeline
        .run(
            &context("\u{25B6}\u{FE0F} **clean item**"),
            &NoRly::default(),
        )
        .expect("pipeline runs");
    // Asserted on the audit trail: `to_construct` is empty in Observe for
    // every message, clean or not, so asserting there would pass regardless.
    assert!(outcome.to_audit().as_slice().is_empty());
}

#[test]
fn the_hook_can_be_bypassed_by_name() {
    // The bypass path is what makes an Observe hook safe to ship: a construct
    // that disagrees with a finding is never stuck behind it.
    let pipeline = production_pipeline().expect("pipeline builds");
    // The pipeline's own API validates that the named hook is installed, so
    // this exercises the real bypass boundary rather than a hand-built one.
    let bypass = pipeline
        .no_rly(&[HookName::parse(STATUS_LINT).expect("valid hook name")])
        .expect("hook is installed");
    let outcome = pipeline
        .run(&context("\u{26A0}\u{FE0F} blocked on #375"), &bypass)
        .expect("pipeline runs");

    let categories: Vec<&str> = outcome
        .to_audit()
        .as_slice()
        .iter()
        .map(|assessment| assessment.category())
        .collect();

    // The same message produces findings without the bypass, so an empty
    // finding set here is the bypass working rather than the lint being quiet.
    assert!(
        categories.contains(&"no-rly-bypass"),
        "bypass was not recorded; got {categories:?}"
    );
    assert!(
        !categories.iter().any(|c| c.starts_with("status-lint/")),
        "bypassed hook still reported; got {categories:?}"
    );
}

/// The scan ceiling has to be observable where the hook actually reports —
/// the audit trail of the shipped pipeline — not only in the unit-level
/// return value of `scan`. The defect this covers made an oversized first
/// line indistinguishable from ordinary prose, and a `scan(...).1` assertion
/// would have stayed green through it.
#[test]
fn the_shipped_pipeline_reports_an_oversized_status_packet_as_truncated() {
    let text = format!("\u{26A0}\u{FE0F} blocked on #375 {}", "x".repeat(200_000));
    let pipeline = production_pipeline().expect("pipeline builds");
    let outcome = pipeline
        .run(&context(&text), &NoRly::default())
        .expect("pipeline runs");

    let categories: Vec<&str> = outcome
        .to_audit()
        .as_slice()
        .iter()
        .map(|assessment| assessment.category())
        .collect();

    assert!(
        categories.contains(&"status-lint/input-truncated"),
        "the ceiling was not reported through the shipped pipeline: {categories:?}"
    );
    // Activation survived the truncation, so the packet was still audited.
    assert!(
        categories.contains(&"status-lint/opaque-id"),
        "activation state was lost with the truncated bytes: {categories:?}"
    );
    // A lint that cannot stop a send is the whole safety claim.
    assert!(matches!(outcome.decision(), HookDecision::Continue));
}

/// An indented example is documentation, not an asserted work state. The
/// shipped pipeline must stay silent on it — including about the `#375` it
/// contains and the URL it visibly carries.
#[test]
fn the_shipped_pipeline_ignores_an_indented_example_packet() {
    let text =
        "how to write one:\n\n    \u{25B6}\u{FE0F} **example** PR #375 https://forge.test/375\n";
    let pipeline = production_pipeline().expect("pipeline builds");
    let outcome = pipeline
        .run(&context(text), &NoRly::default())
        .expect("pipeline runs");

    let attributed: Vec<&str> = outcome
        .to_audit()
        .as_slice()
        .iter()
        .filter_map(|assessment| assessment.hook_name())
        .map(|name| name.as_str())
        .collect();

    assert!(
        !attributed.contains(&STATUS_LINT),
        "an indented example activated the lint: {attributed:?}"
    );
}

/// The quote-scope fix, through the shipped pipeline rather than the unit
/// path. A quoted example must stay exempt, and its unterminated backtick
/// must not reach the asserted status line after it.
#[test]
fn the_shipped_pipeline_reads_the_item_after_a_quoted_backtick() {
    let text = "> quoted `unclosed\n\u{26A0}\u{FE0F} blocked on #375";
    let pipeline = production_pipeline().expect("pipeline builds");
    let outcome = pipeline
        .run(&context(text), &NoRly::default())
        .expect("pipeline runs");

    let categories: Vec<&str> = outcome
        .to_audit()
        .as_slice()
        .iter()
        .map(|assessment| assessment.category())
        .collect();

    assert!(
        categories.contains(&"status-lint/opaque-id"),
        "the quoted backtick masked the real item: {categories:?}"
    );
    assert!(matches!(outcome.decision(), HookDecision::Continue));
}

#[test]
fn the_shipped_pipeline_ignores_a_quoted_example() {
    let text = "> \u{25B6}\u{FE0F} **example** PR #375\nplain prose after";
    let pipeline = production_pipeline().expect("pipeline builds");
    let outcome = pipeline
        .run(&context(text), &NoRly::default())
        .expect("pipeline runs");

    let attributed: Vec<&str> = outcome
        .to_audit()
        .as_slice()
        .iter()
        .filter_map(|assessment| assessment.hook_name())
        .map(|name| name.as_str())
        .collect();

    assert!(
        !attributed.contains(&STATUS_LINT),
        "a quoted example activated the lint: {attributed:?}"
    );
}
