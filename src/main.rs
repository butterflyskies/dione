use color_eyre::eyre::Result;
use std::future::Future;
use tokio::runtime::Builder;

fn main() -> Result<()> {
    color_eyre::install()?;
    run_runtime(async {
        dione::startup::run().await?;
        Ok(())
    })
}

fn run_runtime(operation: impl Future<Output = Result<()>>) -> Result<()> {
    let runtime = Builder::new_multi_thread().enable_all().build()?;
    let result = runtime.block_on(operation);
    // Blocking stdin reads cannot be cancelled even after startup drains owned work.
    runtime.shutdown_background();
    result
}

#[cfg(all(test, unix))]
mod runtime_shutdown_tests {
    use super::run_runtime;
    use std::{
        env,
        path::{Path, PathBuf},
        process::Stdio,
        time::{Duration, Instant},
    };
    use tempfile::TempDir;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{UnixListener, UnixStream},
        process::{Child, Command},
        signal::unix::{SignalKind, signal},
        time::{sleep, timeout},
    };

    const CHILD_DIRECTORY: &str = "DIONE_MAIN_RUNTIME_FIXTURE_DIRECTORY";
    const CHILD_DRAIN: &str = "DIONE_MAIN_RUNTIME_FIXTURE_DRAIN";
    const CHILD_TEST: &str = "runtime_shutdown_tests::signal_with_open_stdin_child";
    const EXIT_BOUND: Duration = Duration::from_secs(5);

    #[test]
    fn signal_with_open_stdin_child() {
        let Some(directory) = env::var_os(CHILD_DIRECTORY) else {
            return;
        };
        let directory = PathBuf::from(directory);
        let drain = env::var_os(CHILD_DRAIN).is_some();
        run_runtime(async move {
            let mut terminate = signal(SignalKind::terminate())?;
            let listener = UnixListener::bind(directory.join("control.sock"))?;
            let (stream, _) = listener.accept().await?;
            let (mut input, mut output) = stream.into_split();
            let completed = directory.join("completed");
            let accepted = tokio::spawn(async move {
                if drain {
                    let mut release = [0_u8; 1];
                    input.read_exact(&mut release).await.unwrap();
                    assert_eq!(release, [b'C']);
                }
                tokio::fs::write(completed, b"accepted work completed").await.unwrap();
            });
            {
                let mut stdin = tokio::io::stdin();
                let mut byte = [0_u8; 1];
                tokio::select! {
                    biased;
                    result = stdin.read(&mut byte) => panic!("stdin must remain pending: {result:?}"),
                    _ = sleep(Duration::from_millis(50)) => {},
                }
            }
            output.write_all(b"R").await?;
            terminate.recv().await.expect("SIGTERM received");
            output.write_all(b"S").await?;
            accepted.await?;
            output.write_all(b"D").await?;
            Ok(())
        }).unwrap();
    }

    async fn marker(stream: &mut UnixStream, expected: u8) {
        let mut byte = [0_u8; 1];
        timeout(Duration::from_secs(5), stream.read_exact(&mut byte))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(byte, [expected]);
    }

    async fn launch(directory: &Path, drain: bool) -> (Child, UnixStream) {
        let stderr = tokio::fs::File::create(directory.join("stderr"))
            .await
            .unwrap()
            .into_std()
            .await;
        let mut command = Command::new(env::current_exe().unwrap());
        command
            .args(["--exact", CHILD_TEST, "--nocapture", "--quiet"])
            .env(CHILD_DIRECTORY, directory)
            .env_remove(CHILD_DRAIN)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr))
            .kill_on_drop(true);
        if drain {
            command.env(CHILD_DRAIN, "1");
        }
        let mut child = command.spawn().unwrap();
        let connected = timeout(Duration::from_secs(5), async {
            loop {
                match UnixStream::connect(directory.join("control.sock")).await {
                    Ok(stream) => break stream,
                    Err(_) => sleep(Duration::from_millis(10)).await,
                }
            }
        })
        .await;
        let stream = match connected {
            Ok(stream) => stream,
            Err(_) => {
                child.kill().await.unwrap();
                child.wait().await.unwrap();
                panic!(
                    "child did not open control socket: {}",
                    tokio::fs::read_to_string(directory.join("stderr"))
                        .await
                        .unwrap()
                );
            }
        };
        (child, stream)
    }

    fn terminate(child: &Child) {
        let pid = i32::try_from(child.id().expect("running child")).unwrap();
        // The owned child remains alive while its parent retains the stdin pipe.
        assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    }

    async fn exited(child: &mut Child, directory: &Path) {
        let status = match timeout(EXIT_BOUND, child.wait()).await {
            Ok(status) => status.unwrap(),
            Err(_) => {
                child.kill().await.unwrap();
                child.wait().await.unwrap();
                panic!(
                    "runtime waited for held-open stdin: {}",
                    tokio::fs::read_to_string(directory.join("stderr"))
                        .await
                        .unwrap()
                );
            }
        };
        assert!(
            status.success(),
            "{}",
            tokio::fs::read_to_string(directory.join("stderr"))
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn sigterm_exits_while_parent_keeps_stdin_open() {
        let directory = TempDir::new().unwrap();
        let (mut child, mut control) = launch(directory.path(), false).await;
        let stdin = child.stdin.take().unwrap();
        marker(&mut control, b'R').await;
        let started = Instant::now();
        terminate(&child);
        marker(&mut control, b'S').await;
        marker(&mut control, b'D').await;
        exited(&mut child, directory.path()).await;
        eprintln!(
            "SIGTERM fixture exit after {:.3} ms",
            started.elapsed().as_secs_f64() * 1000.0
        );
        assert_eq!(
            tokio::fs::read(directory.path().join("completed"))
                .await
                .unwrap(),
            b"accepted work completed"
        );
        drop(stdin);
    }

    #[tokio::test]
    async fn sigterm_waits_for_accepted_work_before_releasing_runtime() {
        let directory = TempDir::new().unwrap();
        let (mut child, mut control) = launch(directory.path(), true).await;
        let stdin = child.stdin.take().unwrap();
        marker(&mut control, b'R').await;
        terminate(&child);
        marker(&mut control, b'S').await;
        assert!(
            timeout(Duration::from_millis(150), child.wait())
                .await
                .is_err()
        );
        assert!(
            !tokio::fs::try_exists(directory.path().join("completed"))
                .await
                .unwrap()
        );
        let started = Instant::now();
        control.write_all(b"C").await.unwrap();
        marker(&mut control, b'D').await;
        exited(&mut child, directory.path()).await;
        eprintln!(
            "accepted drain release to exit after {:.3} ms",
            started.elapsed().as_secs_f64() * 1000.0
        );
        assert_eq!(
            tokio::fs::read(directory.path().join("completed"))
                .await
                .unwrap(),
            b"accepted work completed"
        );
        drop(stdin);
    }
}
