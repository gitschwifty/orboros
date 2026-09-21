use assert_cmd::cargo::cargo_bin_cmd;
use orbs::dep::DepEdge;
use orbs::dep_store::DepStore;
use orbs::orb::{OrbPhase, OrbType};
use orbs::orb_store::OrbStore;
use predicates::str::contains;

#[test]
fn plan_file_respects_shallow_and_status_preserves_the_graph() {
    fn edge_signature(edges: Vec<DepEdge>) -> Vec<(String, String, String)> {
        let mut signature: Vec<_> = edges
            .into_iter()
            .map(|edge| {
                (
                    edge.from.to_string(),
                    edge.to.to_string(),
                    format!("{:?}", edge.edge_type),
                )
            })
            .collect();
        signature.sort();
        signature
    }

    for shallow in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path();
        let spec = state.join("spec.md");
        std::fs::write(&spec, "# Feature\nFirst\nSecond\n").unwrap();
        let mut cmd = cargo_bin_cmd!("orboros");
        cmd.current_dir(state)
            .env("HOME", state)
            .arg("--state-dir")
            .arg(state)
            .args(["plan", "--file"])
            .arg(&spec);
        if shallow {
            cmd.arg("--shallow");
        }
        let phase = if shallow {
            OrbPhase::Decomposing
        } else {
            OrbPhase::Refining
        };
        cmd.assert()
            .success()
            .stdout(contains(format!("{phase:?}")));

        let store = OrbStore::new(state.join("orbs.jsonl"));
        let deps = DepStore::new(state.join("deps.jsonl"));
        let epic = store
            .load_all()
            .unwrap()
            .into_iter()
            .find(|orb| orb.orb_type == OrbType::Epic)
            .unwrap();
        assert_eq!(epic.phase, Some(phase));
        assert_eq!(store.load_children(&epic.id).unwrap().len(), 2);
        let edges = deps.all_edges().unwrap();
        assert_eq!(edges.len(), 5);

        for _ in 0..2 {
            cargo_bin_cmd!("orboros")
                .current_dir(state)
                .env("HOME", state)
                .arg("--state-dir")
                .arg(state)
                .args(["plan", "--status", epic.id.as_str()])
                .assert()
                .success()
                .stdout(contains(format!("{phase:?}")));
        }
        assert_eq!(store.load_all().unwrap().len(), 3);
        assert_eq!(
            edge_signature(deps.all_edges().unwrap()),
            edge_signature(edges)
        );
    }
}
