//! The detector parser and milepost reading.

use railspotting_detect::text::{is_filler, FILLERS, PROMPT};
use railspotting_detect::{all_clear_length, detector_object, ending_text, milepost_from_text, opening_text, parse, jsonfmt,
                          repetitive, wording_text, Assessment};
use serde_json::json;

#[test]
fn parse_defect_detector() {
    let p = parse("Norfolk Southern detector, milepost 476.2, track one, no defects, total axles 312, train speed 46.", true);
    assert_eq!(p, Some(json!({"type": "defect_detector", "railroad": "NS", "milepost": 476.2, "track": 1,
                              "axles": 312, "speed_mph": 46, "defects": false})));
}

#[test]
fn parse_defect_found_and_temperature() {
    let p = parse("CSX detector mile post 102.4 track three, defect, temperature 71", true).unwrap();
    assert_eq!((p["railroad"].as_str(), p["track"].as_i64(), p["defects"].as_bool()), (Some("CSX"), Some(3), Some(true)));
    assert_eq!((p["temperature_f"].as_i64(), p["milepost"].as_f64()), (Some(71), Some(102.4)));
    assert_eq!(parse("NS detector milepost 1.0, temperature -5", true).unwrap()["temperature_f"], -5);
}

#[test]
fn parse_ignores_other_text() {
    assert_eq!(parse("Dispatcher to 21Q, clear signal at Greer, over", true), None);
    assert_eq!(parse("", true), None);
    assert_eq!(parse("detector", true), None); // nothing found in it
}

/// Speech recognition turned a real detector's "no defects" into "so, defect" 4 times in 6.
#[test]
fn parse_from_speech_recognition_never_sets_defects() {
    for text in ["Norfolk Southern, milepost 475.8, so defect, repeat, so defect.",
                 "Norfolk Southern, milepost 475.8, no defects, repeat, no defects."] {
        assert_eq!(parse(text, false), Some(json!({"type": "defect_detector", "railroad": "NS", "milepost": 475.8})));
    }
    assert_eq!(parse("no defects", false), None);
}

#[test]
fn parse_axle_count_needs_total_or_plural() {
    assert_eq!(parse("NS detector milepost 1.0, total axles 312", true).unwrap()["axles"], 312);
    assert_eq!(parse("NS detector milepost 1.0, axles 96", true).unwrap()["axles"], 96);
    assert_eq!(parse("NS detector milepost 1.0, axle count 40", true).unwrap()["axles"], 40);
    assert!(parse("NS detector milepost 1.0, defect, axle 112, south rail", true).unwrap().get("axles").is_none());
}

#[test]
fn milepost_from_digit_by_digit_speech() {
    let cases: [(&str, Option<f64>); 11] = [
        ("Milepost 475.8.", Some(475.8)), ("4, 7, 5, .8.", Some(475.8)), ("475.8.7.", Some(475.8)),
        ("475.87.", Some(475.8)), ("Four seven five point eight seven.", Some(475.8)),
        ("four eight two point one", Some(482.1)), ("milepost 12", Some(12.0)), ("4, 7, 5, 14, 7.", None),
        ("", None), ("point five", None), ("no defects", None)];
    for (text, value) in cases {
        assert_eq!(milepost_from_text(text), value, "{text:?}");
    }
}

#[test]
fn speech_filters() {
    assert!(repetitive("No, no, no. No. No, no."));
    assert!(!repetitive("no defects, repeat, no defects"));
    assert!(is_filler("Thank you.") && is_filler("you") && !is_filler("Thank you, over."));
    assert_eq!(FILLERS.len(), 12);
    assert!(!PROMPT.contains("NOAA") && !PROMPT.to_lowercase().contains("weather"));
}

/// Edge inputs, with the reference values for them.
#[test]
fn edge_inputs_match_reference() {
    let cases: [(&str, Option<f64>); 10] = [
        ("Milepost \u{664}\u{667}\u{665}.\u{668}", Some(475.8)), ("one two three point four five", Some(123.4)),
        ("phone someone 3", Some(113.0)), ("10000.04", Some(10000.0)), ("10000.1", None), ("0.0", Some(0.0)),
        ("oh oh seven", Some(7.0)), ("..5", None), ("5..2", Some(5.2)), ("ohio 12", Some(12.0))];
    for (text, value) in cases {
        assert_eq!(milepost_from_text(text), value, "{text:?}");
    }
    let cases = [
        ("NS detector milepost 12.30 track 2 speed 045 temperature -0",
         r#"{"type":"defect_detector","milepost":12.3,"track":2,"speed_mph":45,"temperature_f":0}"#),
        ("mile post 7 track six, axles count 9", r#"{"type":"defect_detector","milepost":7.0,"track":6,"axles":9}"#),
        ("Detector. No defect. defect", r#"{"type":"defect_detector","defects":false}"#),
        ("Milepost \u{661}\u{662} track one", r#"{"type":"defect_detector","milepost":12.0,"track":1}"#),
    ];
    for (text, want) in cases {
        assert_eq!(jsonfmt::dumps_compact(&parse(text, true).unwrap()), want, "{text:?}");
    }
    let cases = [
        ("CSX, milepost 12.3.  No Defects. repeat", ["CSX, milepost 12.3", "CSX, milepost", "No Defects. repeat"]),
        ("milepost 475.8 , no defect", ["milepost 475.8", "milepost", "no defect"]),
        ("  Norfolk 12 ", ["Norfolk 12", "Norfolk", ""]),
        ("no defects at all\n", ["", "", "no defects at all"]),
        ("abc 4.5.6", ["abc 4.5.6", "abc 4", ""]),
    ];
    for (text, [o, w, e]) in cases {
        assert_eq!((opening_text(text), wording_text(text), ending_text(text)), (o.into(), w.into(), e.into()), "{text:?}");
    }
    let t = |v: serde_json::Value| v.as_object().unwrap().clone();
    assert_eq!(all_clear_length(&t(json!({"length_s": 9.68}))), Some(9.2));
    assert_eq!(all_clear_length(&t(json!({"all_clear_length_s": 0, "length_s": 10.005}))), Some(9.53));
    let a = Assessment { score: 0.555, opening: Some(vec![]), ending: Some(vec![None, Some(0.125)]), rhythm: Some(-0.3),
                         ..Default::default() };
    let o = detector_object("x", "ns", 475.85, "different_message", "opening", &a, 9.005, 9.215, None, Some(""),
                            Some(1791728100.125));
    assert_eq!(jsonfmt::dumps_compact(&o),
               r#"{"detector_id":"x","railroad":"NS","milepost":475.9,"result":"different_message","method":"opening","scores":{"opening":null,"ending":0.12,"rhythm":0.0},"length_s":9.01,"all_clear_length_s":9.21,"repeat_of":null,"heard_text":null,"timestamp":1791728100.12}"#);
}

#[test]
fn spoken_mileposts() {
    use railspotting_detect::text::spoken_milepost;
    for (heard, want) in [
        ("CSX detector, milepost 68.1. No defects.", Some(68.1)),
        ("What's the CSX detector, milepost? Sixteen point one. No defects.", Some(16.1)),
        ("CSX detector milepost six eight point one, no defects", Some(68.1)),
        ("CSX detector milepost sixty eight point one", Some(68.1)),
        ("milepost 6, 8, .1, no defects", Some(68.1)),
        ("Requesting defects detection, milepost, six, eight points, one, no defects", Some(68.1)),
        ("detector, milepost, track six, eight points, one, no defects", Some(68.1)),
        ("defects detected, milepost, 6/8.1, no defects, no defects, total axle, 1/7/4, end of train.", Some(68.1)),
        // Try's all-clears with the csx-edd-v1 profile, 10-14 to 10-22
        ("CFS equipment defect detector, milepost 6.8.1. No defects. No defects. Total axle - 5.6.", Some(68.1)),
        ("TSS. Quickest defect detector, milepost. Six, eight points on. No defects.", Some(68.1)),
        ("BSS equipment defect detector, milepost. 6-8.1.", Some(68.1)),
        ("Norfolk Southern, milepost 475.8.", Some(475.8)),
        ("mile post four seven five point eight", Some(475.8)),
        ("CSX detector, no defects", None),
        ("milepost, no defects", None),
    ] {
        assert_eq!(spoken_milepost(heard), want, "{heard}");
    }
}

#[test]
fn spoken_axles_and_speed() {
    use railspotting_detect::text::parse;
    let p = parse("CSX detector, milepost? Sixteen point one. No defects. No defects. Total axle one, seven, four, \
                   and one train speed.", false).unwrap();
    assert_eq!(p["axles"], 174);
    assert!(p.get("speed_mph").is_none()); // "train speed." with nothing after it
    let p = parse("CSX detector milepost 68.1, no defects, total axles two one six, train speed four five", false).unwrap();
    assert!(p["axles"] == 216 && p["speed_mph"] == 45, "{p}");
    // the digit form still wins, and a lone defective "axle 112" is still not the count
    let p = parse("milepost, 6/8.1, no defects, no defects, total axle, 1/7/4, end of train.", false).unwrap();
    assert_eq!(p["axles"], 174);
    let p = parse("Norfolk Southern detector, milepost 476.2, total axles 312, train speed 46", false).unwrap();
    assert!(p["axles"] == 312 && p["speed_mph"] == 46);
    let p = parse("NS detector milepost 1.0, defect, axle 112, south rail", false).unwrap();
    assert!(p.get("axles").is_none());
}

#[test]
fn spoken_wording_similarity() {
    use railspotting_detect::text::{prompt_from_template, spoken_similarity};
    let ns = "norfolk southern milepost four seven five point eight no defects repeat no defects";
    assert_eq!(spoken_similarity("Norfolk Southern, milepost 475.8, no defects. Repeat, no defects.", ns), 1.0);
    assert!(spoken_similarity("Dispatcher to 21Q, clear signal at Greer, over", ns) < 0.2);
    let csx = "c s x equipment defect detector milepost six eight point one no defects no defects total axles {axles} end of train";
    let heard = "CSX equipment defect detector, milepost 68.1. No defects, no defects. Total axles 1-7-4. End of train.";
    assert_eq!(spoken_similarity(heard, csx), 1.0);
    // what speech made of Try's first recording still clearly matches
    let rough = "DSS analysis with 6 defects detected, milepost, 6/8.1, no defects, no defects, total axle, 1/7/4, end of train.";
    let s = spoken_similarity(rough, csx);
    assert!(s >= 0.6, "{s}");
    assert_eq!(prompt_from_template("CSX equipment detector, milepost {milepost}, track {track}, no defects, total axles {axles}, \
                                     temperature {temperature} degrees, detector out."),
               "CSX equipment detector, milepost, track, no defects, total axles, temperature degrees, detector out.");
}

#[test]
fn decimals_anywhere() {
    use railspotting_detect::text::spoken_decimal;
    assert_eq!(spoken_decimal("Equipment defect, six eight point one, no defects, total axles one seven four"), Some(68.1));
    assert_eq!(spoken_decimal("defect at 68.1, total axles 174"), Some(68.1));
    assert_eq!(spoken_decimal("total axles one seven four, train speed four five"), None);
    // "milepost" heard but its number not: the axle count's misheard decimal is not the milepost
    assert_eq!(spoken_decimal("equipment defect detector, milepost each. No defects. Total axle - 5.6."), None);
    assert_eq!(spoken_decimal("defect detector, 6.8.1. No defects."), Some(68.1));
}

#[test]
fn counts_heard_with_a_decimal_point_are_misheard() {
    use railspotting_detect::text::{spoken_count_after, spoken_value_after};
    assert_eq!(spoken_value_after("Total axle - 5.6. End of transmission.", "total axle", false), None);
    assert_eq!(spoken_count_after("Total axle 7.6 ending of transmission.", &["total axle"]), None);
    assert_eq!(spoken_value_after("Total axle 156. End of transmission.", "total axle", false), Some(156.0));
}
