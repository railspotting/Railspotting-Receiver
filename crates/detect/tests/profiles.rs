//! Detector voice profiles: the built-in files, and reading real transcripts with them.

use railspotting_detect::profile::{default_for, Profile, GENERIC};
use serde_json::json;

fn get(id: &str) -> Profile {
    Profile::builtin().into_iter().find(|p| p.id == id).unwrap_or_else(|| panic!("no built-in {id}"))
}

#[test]
fn builtin_profiles_are_valid_and_unique() {
    let all = Profile::builtin();
    assert!(all.len() >= 3, "{}", all.len());
    for p in &all {
        assert!(p.problems().is_empty(), "{}: {:?}", p.id, p.problems());
        assert!(!p.id.starts_with("custom-"), "{}", p.id);
    }
    let mut ids: Vec<&str> = all.iter().map(|p| p.id.as_str()).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), all.len(), "duplicate profile ids");
    assert_eq!(default_for(&all, "NS").as_deref(), Some("ns-synthetic-v1"));
    assert_eq!(default_for(&all, "csx").as_deref(), Some("csx-edd-v1"));
    assert_eq!(default_for(&all, "BNSF").as_deref(), Some(GENERIC));
}

#[test]
fn prompts_leave_the_values_out() {
    assert_eq!(get("ns-synthetic-v1").prompt(), "Norfolk Southern, milepost, no defects. Repeat, no defects.");
    assert_eq!(get("csx-edd-v1").prompt(),
               "CSX equipment defect detector, milepost. No defects. No defects. Total axle. End of transmission.");
}

#[test]
fn norfolk_southern_readings() {
    let ns = get("ns-synthetic-v1");
    // the filtered, prompted transcript of every recording of the reference detector tested on 10-13
    let r = ns.read("Norfolk Southern, milepost 475.8, no defects. Repeat, no defects.");
    assert!(r.all_clear(&ns) && !r.defect(), "{r:?}");
    assert_eq!(r.fields["milepost"], json!(475.8));
    assert!(r.wording > 0.99);
    // the old number reader's misreads: one out of range, one in range (caught by the nearby list instead)
    let r = ns.read("Norfolk Southern, milepost 4750.8, no defects. Repeat, no defects.");
    assert!(r.fields.get("milepost").is_none() && r.out_of_range["milepost"] == json!(4750.8));
    // "so defects" is how speech hears this voice's "no defects" without a prompt: never a defect
    let r = ns.read("Norfolk Southern, milepost 475.8, so defects. Repeat, so defects.");
    assert!(r.all_clear(&ns) && !r.defect(), "{r:?}");
    // a defect report in this voice's style
    let r = ns.read("Norfolk Southern, milepost 475.8, defect detected. Hot box, axle 112, south rail. Repeat, hot box.");
    assert!(r.defect() && !r.all_clear(&ns), "{r:?}");
    assert!(r.defect_words.iter().any(|w| w == "hot box"));
}

#[test]
fn csx_readings() {
    let csx = get("csx-edd-v1");
    let heard = "CSX equipment defect detector, milepost 68.1. No defects. No defects. Total axle: 1 7 4. End of transmission.";
    let r = csx.read(heard);
    assert!(r.all_clear(&csx), "{r:?}");
    assert!(r.fields["milepost"] == json!(68.1) && r.fields["axles"] == json!(174), "{:?}", r.fields);
    // Try's first recording as speech recognition wrote it on 10-12
    let r = csx.read("DSS analysis with 6 defects detected, milepost, 6/8.1, no defects, no defects, total axle, 1/7/4, end of train.");
    assert!(r.fields["milepost"] == json!(68.1) && r.fields["axles"] == json!(174), "{:?}", r.fields);
    assert_eq!(r.all_clear_said, 2);
    // a real all-clear (heard by ear): the garbled "6 defects detected" is the misheard opening,
    // "CSX equipment defect detector", not a defect report
    assert!(r.all_clear(&csx), "{r:?}");
    // a defect report in CSX style is one
    let r = csx.read("CSX equipment defect detector, milepost 68.1. Defect detected. Hot box, axle 23, north rail. Total axle: 1 7 4.");
    assert!(r.defect() && !r.all_clear(&csx), "{r:?}");
}

#[test]
fn custom_profile_problems() {
    let mut p = get("ns-synthetic-v1");
    p.id = "custom-my test".into();
    p.template = "Norfolk Southern, milepost {milepost}, track {track}".into();
    let problems = p.problems();
    assert!(problems.iter().any(|m| m.starts_with("id:")), "{problems:?}");
    assert!(problems.iter().any(|m| m.contains("{track}")), "{problems:?}");
}

#[test]
fn misheard_all_clear_is_put_right() {
    let ns = get("ns-synthetic-v1");
    assert_eq!(ns.correct("We're here. Norfolk's under milepost 475.8, so defects. Repeat, so defects."),
               "We're here. Norfolk's under milepost 475.8, no defects. Repeat, no defects.");
    assert_eq!(ns.correct("So defects. Repeat, so defect."), "No defects. Repeat, no defects.");
    // only the phrase as a whole: other words with "so" stay
    assert_eq!(ns.correct("also defective, so far"), "also defective, so far");
}

#[test]
fn no_heard_as_a_zero_after_the_milepost() {
    let ns = get("ns-synthetic-v1");
    // small.en on a real all-clear of the reference detector, 10-13: the "no" of "no defects" became a trailing 0
    let heard = "Norfolk Southern, milepost 475.80 defects. Repeat, no defects.";
    let fixed = ns.correct(heard);
    assert_eq!(fixed, "Norfolk Southern, milepost 475.8, no defects. Repeat, no defects.");
    let r = ns.read(heard);
    assert!(r.all_clear(&ns) && !r.defect() && r.fields["milepost"] == json!(475.8), "{r:?}");
    // the same with the zero, "oh" or "zero" written apart
    for h in ["milepost 475.8 0 defects", "milepost 475.8, oh defects", "milepost 475.8 zero defects"] {
        assert_eq!(ns.correct(h), "milepost 475.8, no defects", "{h}");
    }
    // a value with its one decimal and nothing after it is left alone
    assert_eq!(ns.correct("milepost 475.8, no defects"), "milepost 475.8, no defects");
    assert_eq!(ns.correct("milepost 402.0 hot box"), "milepost 402.0 hot box");
    // CSX: "defect detector" after a decimal is not an all-clear
    let csx = get("csx-edd-v1");
    assert_eq!(csx.correct("milepost 68.10 defects, no defects"), "milepost 68.1, no defects, no defects");
}
