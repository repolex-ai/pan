//! One `pan:Instance` per store, written by the daemon at open (pan issue
//! #28; goodlux, 2026-09-17: the Instance is whatever pand is running over).

use pan::instance::InstanceFacts;
use pan::{Pan, QueryResults};

fn facts(id: &str, port: u16) -> InstanceFacts {
    InstanceFacts {
        id: id.to_string(),
        base_url: format!("http://127.0.0.1:{port}"),
        listen_port: port,
    }
}

fn instances(store: &Pan) -> Vec<String> {
    match store
        .query("SELECT ?s WHERE { ?s a pan:Instance }")
        .unwrap()
    {
        QueryResults::Solutions(sols) => sols
            .filter_map(|r| r.ok())
            .map(|r| r.get("s").unwrap().to_string())
            .collect(),
        _ => panic!("not a select"),
    }
}

fn fields(store: &Pan, iri: &str) -> Vec<(String, String)> {
    let q = format!("SELECT ?p ?o WHERE {{ <{iri}> ?p ?o }}");
    match store.query(&q).unwrap() {
        QueryResults::Solutions(sols) => sols
            .filter_map(|r| r.ok())
            .map(|r| {
                (
                    r.get("p").unwrap().to_string(),
                    r.get("o").unwrap().to_string(),
                )
            })
            .collect(),
        _ => panic!("not a select"),
    }
}

#[test]
fn a_store_carries_exactly_one_instance_with_the_declared_fields_spelled_pan() {
    let dir = tempfile::tempdir().unwrap();
    let store = Pan::open(dir.path()).unwrap();
    assert!(
        instances(&store).is_empty(),
        "a bare open writes no Instance"
    );

    store.declare_instance(&facts("mac-studio", 7401)).unwrap();
    let found = instances(&store);
    assert_eq!(found, vec!["<https://repolex.ai/pan/Instance/mac-studio>"]);

    let iri = "https://repolex.ai/pan/Instance/mac-studio";
    let f = fields(&store, iri);
    let pan = "https://repolex.ai/ontology/pan/";
    let get = |local: &str| -> Vec<String> {
        f.iter()
            .filter(|(p, _)| p == &format!("<{pan}{local}>"))
            .map(|(_, o)| o.clone())
            .collect()
    };
    assert_eq!(
        get("id"),
        vec![format!("<{iri}>")],
        "pan:id is the node itself"
    );
    assert_eq!(get("createdDate").len(), 1);
    assert_eq!(
        get("fsRoot"),
        vec![format!("\"{}\"", store.layout.media_root.display())]
    );
    assert_eq!(get("instanceMode"), vec!["\"managed\""]);
    assert_eq!(get("sourceFormat"), vec!["\"image/png\""]);
    assert_eq!(
        get("listenPort"),
        vec!["\"7401\"^^<http://www.w3.org/2001/XMLSchema#integer>"]
    );
    assert_eq!(
        get("primaryGraph"),
        vec![format!(
            "\"http://127.0.0.1:7401/stores/{}/sparql\"",
            store.store_id
        )]
    );
    assert!(get("localGraph").is_empty(), "pand keeps no cache graph");
    for (p, _) in &f {
        assert!(
            p.contains("/ontology/pan/") || p.contains("rdf-syntax-ns#type"),
            "every Instance predicate is pan: — got {p}"
        );
    }
}

#[test]
fn a_second_open_leaves_exactly_one_instance_and_keeps_its_creation_date() {
    let dir = tempfile::tempdir().unwrap();
    let created = {
        let store = Pan::open(dir.path()).unwrap();
        store.declare_instance(&facts("mac-studio", 7401)).unwrap();
        fields(&store, "https://repolex.ai/pan/Instance/mac-studio")
            .into_iter()
            .find(|(p, _)| p.ends_with("/createdDate>"))
            .map(|(_, o)| o)
            .unwrap()
    };
    let store = Pan::open(dir.path()).unwrap();
    // Same machine, the port moved: the record is rewritten, not doubled.
    store.declare_instance(&facts("mac-studio", 7402)).unwrap();
    assert_eq!(instances(&store).len(), 1);
    let f = fields(&store, "https://repolex.ai/pan/Instance/mac-studio");
    assert!(f
        .iter()
        .any(|(p, o)| p.ends_with("/listenPort>") && o.starts_with("\"7402\"")));
    assert!(f
        .iter()
        .any(|(p, o)| p.ends_with("/createdDate>") && o == &created));
    // The machine renamed: the old node goes, still exactly one.
    store.declare_instance(&facts("new-name", 7402)).unwrap();
    assert_eq!(
        instances(&store),
        vec!["<https://repolex.ai/pan/Instance/new-name>"]
    );
}
