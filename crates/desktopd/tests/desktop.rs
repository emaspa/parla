use desktopd::DesktopIndex;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "parla-desktop-regression-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn entry(&self, relative: &str, name: &str, extra: &str) {
        let path = self.0.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            path,
            format!("[Desktop Entry]\nType=Application\nName={name}\n{extra}\n"),
        )
        .unwrap();
    }

    fn index(&self) -> DesktopIndex {
        DesktopIndex::from_dirs(std::slice::from_ref(&self.0))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn lookup_requires_content_tokens() {
    let fixture = Fixture::new();
    fixture.entry(
        "applications/partitionmanager.desktop",
        "KDE Partition Manager",
        "",
    );
    let index = fixture.index();
    for query in ["kate", "", "   ", "please the a an up for me"] {
        assert!(
            index.lookup(query).is_none(),
            "unexpected match for {query:?}"
        );
    }
    fixture.entry("applications/firefox.desktop", "Firefox", "");
    fixture.entry("applications/thunderbird.desktop", "Thunderbird", "");
    let index = fixture.index();
    for query in [
        "firefox",
        "fire fox",
        "FIREFOX please",
        "the firefox for me",
        "firefox unrelated",
    ] {
        if query.ends_with("unrelated") {
            assert!(index.lookup(query).is_none());
        } else {
            assert_eq!(index.lookup(query).unwrap().name, "Firefox", "{query}");
        }
    }
    assert!(index.shortlist("the", 8).is_empty());
    assert!(index
        .shortlist("what is the weather tomorrow", 8)
        .is_empty());
    assert_eq!(
        index.shortlist("bring up fire fox for me", 8)[0].name,
        "Firefox"
    );
    assert_eq!(index.by_id("firefox.desktop").unwrap().name, "Firefox");
    assert!(index.by_id("firefox").is_none());
}

#[test]
fn lookup_checks_keywords_after_a_weak_name_match() {
    let fixture = Fixture::new();
    fixture.entry(
        "applications/editor.desktop",
        "A long long long B",
        "Keywords=ab;",
    );
    assert_eq!(fixture.index().lookup("ab").unwrap().id, "editor.desktop");
}

#[test]
fn nested_ids_and_data_directory_precedence() {
    let fixture = Fixture::new();
    fixture.entry("user/applications/kde4/foo.desktop", "User Foo", "");
    fixture.entry("system/applications/kde4-foo.desktop", "System Foo", "");
    fixture.entry("system/applications/foo.desktop", "Plain Foo", "");
    fixture.entry(
        "flatpak/exports/share/applications/org.test.App.desktop",
        "Flatpak App",
        "",
    );
    let index = DesktopIndex::from_dirs(&[
        fixture.0.join("user"),
        fixture.0.join("system"),
        fixture.0.join("flatpak/exports/share"),
    ]);
    assert_eq!(index.entries().len(), 3);
    assert_eq!(
        index
            .entries()
            .iter()
            .find(|e| e.id == "kde4-foo.desktop")
            .unwrap()
            .name,
        "User Foo"
    );
    assert!(index.lookup("Flatpak App").is_some());
}

#[test]
fn desktop_visibility_and_try_exec() {
    let fixture = Fixture::new();
    fixture.entry("applications/link.desktop", "Link", "Type=Link");
    fixture.entry(
        "applications/directory.desktop",
        "Directory",
        "Type=Directory",
    );
    fixture.entry("applications/hidden.desktop", "Hidden", "Hidden=true");
    fixture.entry(
        "applications/nodisplay.desktop",
        "NoDisplay",
        "NoDisplay=true",
    );
    fixture.entry(
        "applications/missing.desktop",
        "Missing",
        "TryExec=parla-regression-missing-binary-93cc122c",
    );
    fixture.entry("applications/available.desktop", "Available", "TryExec=sh");
    let index = fixture.index();
    assert_eq!(
        index
            .entries()
            .iter()
            .map(|e| e.name.as_str())
            .collect::<Vec<_>>(),
        ["Available"]
    );
}

#[test]
fn hidden_nested_override_masks_system_entry() {
    let fixture = Fixture::new();
    fixture.entry(
        "user/applications/kde4/foo.desktop",
        "Hidden",
        "Hidden=true",
    );
    fixture.entry("system/applications/kde4-foo.desktop", "System Foo", "");
    let index = DesktopIndex::from_dirs(&[fixture.0.join("user"), fixture.0.join("system")]);
    assert!(index.entries().is_empty());
}

#[test]
#[cfg(unix)]
fn try_exec_requires_an_executable_file() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let binary = fixture.0.join("program");
    std::fs::write(&binary, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o644)).unwrap();
    fixture.entry(
        "applications/app.desktop",
        "App",
        &format!("TryExec={}", binary.display()),
    );
    assert!(fixture.index().entries().is_empty());
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(fixture.index().entries().len(), 1);
    std::fs::remove_file(&binary).unwrap();
    std::fs::create_dir(&binary).unwrap();
    assert!(fixture.index().entries().is_empty());
}
