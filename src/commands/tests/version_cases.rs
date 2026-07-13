use super::*;

#[test]
fn extract_version_patterns_handles_git_and_release_urls() {
    let git_patterns = extract_version_patterns("https://codeberg.org/Limine/limine.git#v$version");
    assert!(git_patterns.contains(&VersionPattern {
        prefix: "v".into(),
        suffix: String::new(),
    }));

    let release_patterns = extract_version_patterns(
        "https://github.com/Mic92/iana-etc/releases/download/$version/iana-etc-$version.tar.gz",
    );
    assert!(release_patterns.contains(&VersionPattern {
        prefix: String::new(),
        suffix: String::new(),
    }));
}

#[test]
fn candidate_versions_from_refs_matches_version_patterns() {
    let refs = vec![
        "refs/tags/v10.8.3".to_string(),
        "refs/tags/v10.8.4".to_string(),
        "refs/heads/main".to_string(),
    ];
    let patterns = extract_version_patterns("https://codeberg.org/Limine/limine.git#v$version");
    let candidates = candidate_versions_from_refs(&refs, &patterns);

    assert_eq!(candidates, vec!["10.8.3".to_string(), "10.8.4".to_string()]);
    assert_eq!(
        best_newer_version("10.8.3", candidates.iter().map(String::as_str)),
        Some("10.8.4".to_string())
    );
}

#[test]
fn best_newer_version_skips_branches_and_prereleases() {
    let candidates = ["2", "1.10.0rc1", "1.10.0", "release-0.13"];
    assert_eq!(
        best_newer_version("1.9.5", candidates.into_iter()),
        Some("1.10.0".to_string())
    );
}

#[test]
fn best_newer_version_normalizes_date_style_tags() {
    let candidates = ["lts_2026_01_07", "lts_2027_02_03"];
    assert_eq!(
        best_newer_version("20260107.1", candidates.into_iter()),
        Some("20270203".to_string())
    );
}

#[test]
fn remote_git_repository_from_github_release_url_maps_to_repo_git_url() {
    let repo_url = remote_git_repository_from_source_url(
        "https://github.com/Mic92/iana-etc/releases/download/20260202/iana-etc-20260202.tar.gz",
    );
    assert_eq!(
        repo_url,
        Some("https://github.com/Mic92/iana-etc.git".to_string())
    );
}

#[test]
fn remote_git_repository_from_gitlab_archive_url_maps_to_repo_git_url() {
    let repo_url = remote_git_repository_from_source_url(
        "https://gitlab.com/graphviz/graphviz/-/archive/14.1.4/graphviz-14.1.4.tar.gz",
    );
    assert_eq!(
        repo_url,
        Some("https://gitlab.com/graphviz/graphviz.git".to_string())
    );
}

#[test]
fn archive_listing_probe_uses_parent_of_first_version_segment() {
    let probe = archive_listing_probe(
        "https://downloads.example.test/dav1d/$version/dav1d-$version.tar.xz",
        "https://downloads.example.test/dav1d/1.5.3/dav1d-1.5.3.tar.xz",
    )
    .expect("archive probe");
    assert_eq!(probe.listing_url, "https://downloads.example.test/dav1d/");
    assert_eq!(
        probe.patterns,
        vec![VersionPattern {
            prefix: String::new(),
            suffix: String::new(),
        }]
    );
}

#[test]
fn candidate_versions_from_listing_matches_archive_entries() {
    let patterns = vec![VersionPattern {
        prefix: "alsa-lib-".into(),
        suffix: ".tar.bz2".into(),
    }];
    let html = r#"
            <a href="alsa-lib-1.2.15.3.tar.bz2">alsa-lib-1.2.15.3.tar.bz2</a>
            <a href="alsa-lib-1.2.16.tar.bz2">alsa-lib-1.2.16.tar.bz2</a>
        "#;
    assert_eq!(
        candidate_versions_from_listing(html, &patterns),
        vec!["1.2.15.3".to_string(), "1.2.16".to_string()]
    );
}

#[test]
fn list_archive_versions_reads_simple_http_index() -> Result<()> {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::thread;

    let listener = TcpListener::bind("127.0.0.1:0").context("bind test listener")?;
    let addr = listener.local_addr().context("listener addr")?;
    let server = thread::spawn(move || -> Result<()> {
        let (mut stream, _) = listener.accept().context("accept request")?;
        let mut reader = BufReader::new(stream.try_clone().context("clone stream")?);
        let mut request_line = String::new();
        reader
            .read_line(&mut request_line)
            .context("read request line")?;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).context("read header line")?;
            if line == "\r\n" || line.is_empty() {
                break;
            }
        }
        assert!(request_line.starts_with("GET /pub/lib/ HTTP/1.1"));
        let body = r#"
                <html>
                    <a href="alsa-lib-1.2.15.3.tar.bz2">alsa-lib-1.2.15.3.tar.bz2</a>
                    <a href="alsa-lib-1.2.16.tar.bz2">alsa-lib-1.2.16.tar.bz2</a>
                </html>
            "#;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .context("write response")?;
        stream.flush().context("flush response")?;
        Ok(())
    });

    let probe = ArchiveListingProbe {
        listing_url: format!("http://{addr}/pub/lib/"),
        patterns: vec![VersionPattern {
            prefix: "alsa-lib-".into(),
            suffix: ".tar.bz2".into(),
        }],
    };
    let versions = list_archive_versions(&probe)?;
    server.join().expect("join server")?;
    assert_eq!(versions, vec!["1.2.15.3".to_string(), "1.2.16".to_string()]);
    Ok(())
}
