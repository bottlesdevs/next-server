use std::path::PathBuf;
use std::time::Duration;

use bottles_core::layers::{LayerManager, Tools};
use bottles_core::proto::bottles_client::BottlesClient;
use bottles_core::proto::bottles_server::BottlesServer;
use bottles_core::proto::{self, Layer};
use bottles_server::BottlesService;

const BASE_REG: &str = "WINE REGISTRY Version 2\n;; All keys relative to REGISTRY\\\\Machine\n\n[Software\\\\ToDelete] 1742032912\n#time=1db959146b5541a\n\"x\"=\"y\"\n\n[Software\\\\ToUpdate] 1742032912\n#time=1db959146b5541a\n\"Ver\"=\"1.0\"\n";
const POST_REG: &str = "WINE REGISTRY Version 2\n;; All keys relative to REGISTRY\\\\Machine\n\n[Software\\\\ToUpdate] 1742032912\n#time=1db959146b5541a\n\"Ver\"=\"2.0\"\n\n[Software\\\\NewDep] 1742032912\n#time=1db959146b5541a\n\"Installed\"=\"yes\"\n";

fn tools_configured() -> bool {
    ["FVS2D_BIN", "FVS2_BIN", "REGDIFF_BIN"].iter().all(|v| std::env::var_os(v).is_some())
}

fn layer(repo: &std::path::Path) -> Layer {
    Layer { repo: repo.display().to_string(), state: None }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prepare_capture_replay_over_grpc() {
    if !tools_configured() {
        eprintln!("skipping prepare_capture_replay_over_grpc: set FVS2D_BIN/FVS2_BIN/REGDIFF_BIN");
        return;
    }

    let work = std::env::temp_dir().join(format!("grpc-layers-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).unwrap();

    // Base "Virgo" prefix committed directly through the library.
    let virgo = work.join("virgo");
    std::fs::create_dir_all(virgo.join("system32")).unwrap();
    std::fs::write(virgo.join("system32/core.dll"), b"core").unwrap();
    std::fs::write(virgo.join("system.reg"), BASE_REG).unwrap();
    LayerManager::new(Tools::from_env()).commit_layer(&virgo, "virgo").unwrap();

    // Start the server on an ephemeral port.
    let addr = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap()
    };
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(BottlesServer::new(BottlesService::default()))
            .serve(addr)
            .await
            .unwrap();
    });

    // Connect (retry until the server is listening).
    let endpoint = format!("http://{addr}");
    let mut client = loop {
        match BottlesClient::connect(endpoint.clone()).await {
            Ok(c) => break c,
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    };

    // 1) Prepare a writable prefix over Virgo; the upper dir becomes the new layer.
    let dep = work.join("dep");
    let mnt1 = work.join("mnt1");
    let prepared = client
        .prepare_prefix(proto::PreparePrefixRequest {
            lowers: vec![layer(&virgo)],
            upper: dep.display().to_string(),
            mountpoint: mnt1.display().to_string(),
        })
        .await
        .unwrap()
        .into_inner();

    // 2) "Install" through the mount: a new file plus registry changes.
    let mp = PathBuf::from(&prepared.mountpoint);
    std::fs::write(mp.join("system32/newdep.dll"), b"newdep").unwrap();
    std::fs::write(mp.join("system.reg"), POST_REG).unwrap();

    // 3) Release (unmount), then capture the upper as a layer.
    client
        .release_prefix(proto::ReleasePrefixRequest { session_id: prepared.session_id })
        .await
        .unwrap();
    let captured = client
        .capture_layer(proto::CaptureLayerRequest {
            upper: dep.display().to_string(),
            base_dir: virgo.display().to_string(),
            message: "dep".to_string(),
        })
        .await
        .unwrap()
        .into_inner();
    assert!(captured.ok);

    // 4) Replay: prepare Virgo + the captured dep over a fresh upper.
    let mnt2 = work.join("mnt2");
    let replay = client
        .prepare_prefix(proto::PreparePrefixRequest {
            lowers: vec![layer(&virgo), layer(&dep)],
            upper: work.join("upper2").display().to_string(),
            mountpoint: mnt2.display().to_string(),
        })
        .await
        .unwrap()
        .into_inner();

    let merged = std::fs::read_to_string(PathBuf::from(&replay.mountpoint).join("system.reg")).unwrap();
    assert!(merged.contains("NewDep"), "merged registry should gain NewDep");
    assert!(merged.contains("\"Ver\"=\"2.0\""), "ToUpdate should be 2.0");
    assert!(!merged.contains("ToDelete"), "ToDelete should be gone");
    assert_eq!(
        std::fs::read_to_string(PathBuf::from(&replay.mountpoint).join("system32/newdep.dll")).unwrap(),
        "newdep"
    );

    client
        .release_prefix(proto::ReleasePrefixRequest { session_id: replay.session_id })
        .await
        .unwrap();

    let _ = std::fs::remove_dir_all(&work);
}
