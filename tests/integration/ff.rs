//! Fake Firefox profile with a Moodle session (for the shots 409 precheck).
use std::path::Path;

pub fn firefox_session(root: &Path) {
    let j = br#"{"cookies":[{"host":".educaciodigital.cat","name":"MoodleSessionx","value":"v"}]}"#;
    let mut b = b"mozLz40\0".to_vec();
    b.extend((j.len() as u32).to_le_bytes());
    b.push(0xF0); // one literal run, length 15 + extension bytes
    let mut r = j.len() - 15;
    while r >= 255 {
        b.push(255);
        r -= 255;
    }
    b.push(r as u8);
    b.extend(j);
    let sb = root.join("p/sessionstore-backups");
    std::fs::create_dir_all(&sb).unwrap();
    std::fs::write(root.join("installs.ini"), "[A]\nDefault=p\n").unwrap();
    std::fs::write(sb.join("recovery.jsonlz4"), b).unwrap();
}
