use super::*;
use crate::picker::{testing::*, Outcome};
#[test]
fn groups_preserve_distance_counts_filter_zones_and_back_stack() {
    let zones = [
        "us-west4-a",
        "us-west1-c",
        "us-east5-b",
        "us-east1-d",
        "asia-south1-b",
    ]
    .into_iter()
    .map(|s| Zone {
        name: s.into(),
        types: Vec::new(),
    })
    .collect::<Vec<_>>();
    let job = Job::new({
        let zones = zones.clone();
        move || Ok(zones.clone())
    });
    let result = drive(screen(job, zones, "TPU zone", "", Next::Done), |keys| {
        keys.wait(|s| s.labels.len() == 3);
        assert_eq!(
            keys.labels(),
            [
                "us-west     2  zones",
                "us-east     2  zones",
                "asia-south  1  zone"
            ]
        );
        keys.press_until(RIGHT, on("us-west zones"));
        assert_eq!(keys.labels(), ["us-west4-a", "us-west1-c"]);
        keys.press_until(BACK, on("TPU zone"));
        keys.type_text("us-east1-d");
        assert_eq!(keys.labels(), ["us-east1-d"]);
        keys.press(RIGHT);
    });
    assert_eq!(result, Outcome::Done("us-east1-d".into()));
}
#[test]
fn preferred_zone_is_highlighted_without_changing_distance_order() {
    let zones = ["us-west4-a", "us-west1-c", "us-east5-b"]
        .into_iter()
        .map(|s| Zone {
            name: s.into(),
            types: Vec::new(),
        })
        .collect::<Vec<_>>();
    let job = Job::new({
        let zones = zones.clone();
        move || Ok(zones.clone())
    });
    let result = drive(
        present(
            job,
            zones,
            "TPU zone",
            "",
            Presentation {
                preferred: Some("us-west1-c".into()),
                ..Presentation::default()
            },
            Next::Done,
        ),
        |keys| {
            keys.press_until(RIGHT, on("us-west zones"));
            assert_eq!(keys.labels(), ["us-west4-a", "us-west1-c"]);
            assert!(keys
                .snapshot()
                .body
                .iter()
                .any(|line| line.starts_with('›') && line.contains("us-west1-c")));
            keys.press(RIGHT);
        },
    );
    assert_eq!(result, Outcome::Done("us-west1-c".into()));
}
#[test]
fn area_names_come_from_region_names() {
    for (zone, wanted) in [
        ("us-east5-b", "us-east"),
        ("northamerica-northeast2-a", "northamerica-northeast"),
        ("southamerica-west1-c", "southamerica"),
        ("southamerica-east1-c", "southamerica"),
        ("new-place17-ai1b", "new-place"),
    ] {
        assert_eq!(area(zone), wanted);
    }
}
