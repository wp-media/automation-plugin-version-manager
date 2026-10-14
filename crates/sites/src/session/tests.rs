//! Sessions over [`FakeTransport`]: commands run as the probed identity,
//! the transport's truncation reaches the parsers, plugin lists are bound
//! to their session, and transport failures keep their codes.

use super::*;
use crate::error::Code;
use crate::remote::fake::{FakeTransport, Reply};
use crate::remote::{BoxFuture, Captured, Delivery, Execution, Request, STDOUT_LIMIT};
use crate::wp::OutputDefect;
use crate::wp::probe::tests::{Host, happy, site};

/// `plugin list`'s output on the harness's `single` site.
const LIST: &str = include_str!("../../tests/fixtures/wp/plugin-list-single.json");

/// A session opened against the fake, with `more` replies after the
/// probe's.
///
/// # Arguments
///
/// * `fake` - The transport (its replies start with the probe's)
async fn open(fake: &FakeTransport) -> Session<'_> {
    Session::open(fake, &site(|_| {})).await.unwrap()
}

/// The fake's replies: a probe, then `more`.
///
/// # Arguments
///
/// * `more` - The replies after the probe
fn replies(more: impl IntoIterator<Item = Reply>) -> Vec<Reply> {
    let mut replies = happy(&Host::deploy());
    replies.extend(more);
    replies
}

/// The script text of the fake's last call.
///
/// # Arguments
///
/// * `fake` - The transport
fn last_script(fake: &FakeTransport) -> String {
    match fake.calls().last().map(Request::delivery) {
        Some(Delivery::Stdin(script)) => script.text().to_string(),
        other => panic!("no stdin script: {other:?}"),
    }
}

#[tokio::test]
async fn commands_run_as_the_probed_identity() {
    let fake = FakeTransport::new(replies([Reply::status(1, "", "Error: nope\n")]));
    let session = open(&fake).await;
    assert_eq!(session.site_id(), "single");
    assert_eq!(
        session.identity(),
        &Identity::sudo("www-data", Some("www-data")).unwrap()
    );
    let run = session.run(&WpCommand::boot_check()).await.unwrap();
    // A non-zero exit is a result: the parser decides.
    assert_eq!((run.kind(), run.exit_code()), (CommandKind::BootCheck, 1));
    assert_eq!(run.output().stderr(), b"Error: nope\n");
    let script = last_script(&fake);
    assert!(script.contains("sudo -n -u 'www-data' -g 'www-data' -- env -u EDITOR"));
    assert!(script.contains("--url='http://single.test' 'option' 'get' 'siteurl'"));
    assert_eq!(
        fake.calls().last().unwrap().timeout(),
        crate::wp::probe::DEFAULT_TIMEOUT
    );
    assert!(format!("{session:?}").contains("single"));
}

#[tokio::test]
async fn plugin_lists_are_bound_to_their_session() {
    let fake = FakeTransport::new(replies([Reply::ok(LIST)]));
    let session = open(&fake).await;
    let plugins = session.plugin_list().await.unwrap();
    assert_eq!(plugins.site_id(), "single");
    assert!(plugins.notices().is_empty());
    let akismet = Slug::new("akismet").unwrap();
    let target = plugins.resolve(&akismet).unwrap();
    assert_eq!(target.slug(), &akismet);
    let resolved = session.target(&target).unwrap();
    assert_eq!(resolved.row().version(), Some("5.7.2"));
    // Must-use rows never resolve for a command.
    assert!(
        plugins
            .resolve(&Slug::new("apvm-e2e-mu").unwrap())
            .is_none()
    );
    assert!(plugins.list().rows().len() >= 4);

    // Another session (another probe of the same site) refuses it.
    let other_fake = FakeTransport::new(happy(&Host::deploy()));
    let other = open(&other_fake).await;
    let error = other.target(&target).unwrap_err();
    assert_eq!(error.code(), Code::InvalidInput);
    assert_eq!(
        error.to_string(),
        "invalid plugin target: target: 'akismet' was resolved in a plugin list of another \
         session, not site 'single''s"
    );
}

#[tokio::test]
async fn regular_plugins_are_bound_too() {
    let regular = include_str!("../../tests/fixtures/wp/regular-plugins-single.json");
    let fake = FakeTransport::new(replies([Reply::ok(regular)]));
    let session = open(&fake).await;
    let plugins = session.regular_plugins().await.unwrap();
    let target = plugins.resolve(&Slug::new("akismet").unwrap()).unwrap();
    assert!(session.target(&target).is_ok());
    assert!(last_script(&fake).contains("'eval'"));
}

#[tokio::test]
async fn unreadable_lists_and_transport_failures_are_errors() {
    let fake = FakeTransport::new(replies([
        Reply::ok("<html>maintenance</html>"),
        Reply::status(255, "", "Connection to x closed by remote host.\r\n"),
        Reply::timed_out(""),
    ]));
    let session = open(&fake).await;
    assert_eq!(
        session.plugin_list().await.unwrap_err().code(),
        Code::WpCommandFailed
    );
    assert_eq!(
        session.plugin_list().await.unwrap_err().code(),
        Code::SshConnectionLost
    );
    assert_eq!(
        session.plugin_list().await.unwrap_err().code(),
        Code::Timeout
    );
}

/// A transport that answers every call with a stdout cut at its limit
/// (after a complete-looking list).
struct CutStdout;

impl Transport for CutStdout {
    /// The nonce line and a list, flagged truncated.
    fn execute<'a>(&'a self, request: &'a Request) -> BoxFuture<'a, crate::Result<Execution>> {
        let mut stdout = format!("\n{}\n", request.nonce()).into_bytes();
        stdout.extend(LIST.as_bytes());
        Box::pin(async move {
            Ok(Execution::new(
                request.nonce().clone(),
                Exit::Status(0),
                Captured::new(stdout, true),
                Captured::default(),
                true,
                Duration::from_millis(1),
            ))
        })
    }
}

#[tokio::test]
async fn a_cut_stdout_reaches_the_parser_as_truncated() {
    let fake = FakeTransport::new(happy(&Host::deploy()));
    let probe = probe::probe(&fake, &site(|_| {})).await.unwrap();
    let session = Session::new(&CutStdout, probe, Duration::from_secs(5));
    let run = session.run(&WpCommand::plugin_list()).await.unwrap();
    assert!(run.output().stdout_truncated());
    assert!(run.output().stdout().starts_with(b"[{"));
    assert!(STDOUT_LIMIT > LIST.len());
    let error = session.plugin_list().await.unwrap_err();
    assert!(
        matches!(
            error,
            Error::WpOutput {
                defect: OutputDefect::Truncated { .. },
                ..
            }
        ),
        "{error}"
    );
}

#[tokio::test]
async fn commands_naming_a_plugin_run_only_on_a_bound_target() {
    let fake = FakeTransport::new(replies([Reply::ok(LIST), Reply::ok("")]));
    let session = open(&fake).await;
    let plugins = session.plugin_list().await.unwrap();
    let akismet = Slug::new("akismet").unwrap();
    let target = plugins.resolve(&akismet).unwrap();
    let calls = fake.calls().len();

    // A list parsed outside the session cannot drive a mutation.
    let foreign = crate::wp::parse::plugin_list(&CommandOutput::new(0, LIST.as_bytes(), b""))
        .unwrap()
        .value;
    let unbound = foreign.resolve(&akismet).unwrap();
    let error = session
        .run(&WpCommand::activate(&unbound, ActivationScope::Site))
        .await
        .unwrap_err();
    assert_eq!(error.code(), Code::InvalidInput);
    assert!(
        error
            .to_string()
            .contains("'wp plugin activate' names a plugin"),
        "{error}"
    );
    assert_eq!(fake.calls().len(), calls, "nothing ran");

    // The bound target runs.
    let run = session
        .run_on(&target, TargetOp::Activate(ActivationScope::Site))
        .await
        .unwrap();
    assert_eq!(run.kind(), CommandKind::Activate);
    assert!(last_script(&fake).contains("'plugin' 'activate' 'akismet'"));

    // Another session's target is refused before anything runs.
    let other_fake = FakeTransport::new(happy(&Host::deploy()));
    let other = open(&other_fake).await;
    let error = other.run_on(&target, TargetOp::Delete).await.unwrap_err();
    assert_eq!(error.code(), Code::InvalidInput);
    assert_eq!(other_fake.remaining(), 0);
}

#[test]
fn every_target_op_builds_its_command() {
    let list = crate::wp::parse::plugin_list(&CommandOutput::new(0, LIST.as_bytes(), b""))
        .unwrap()
        .value;
    let target = list.resolve(&Slug::new("akismet").unwrap()).unwrap();
    let cases = [
        (TargetOp::RequiresPlugins, CommandKind::RequiresPlugins),
        (
            TargetOp::Activate(ActivationScope::Site),
            CommandKind::Activate,
        ),
        (
            TargetOp::Deactivate(ActivationScope::Site, DeactivateMode::Load),
            CommandKind::Deactivate,
        ),
        (TargetOp::Uninstall, CommandKind::Uninstall),
        (TargetOp::Reset, CommandKind::Reset),
        (TargetOp::Delete, CommandKind::Delete),
    ];
    for (op, kind) in cases {
        let command = op.command(&target);
        assert_eq!(command.kind(), kind);
        assert!(kind.targets_plugin());
    }
    for kind in [
        CommandKind::PluginList,
        CommandKind::BootCheck,
        CommandKind::Install,
    ] {
        assert!(!kind.targets_plugin());
    }
}
