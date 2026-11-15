# Changesets for Ferune

Ferune is a Rust application, so releases use Changesets-format Markdown files
to drive Cargo versioning and GitHub Releases; no npm package is published.

Add one `.md` file in this directory with each user-visible change:

```md
---
"ferune": patch
---

Fix how an image preview handles rotated photos.
```

Use `patch` for fixes, `minor` for backwards-compatible features, and `major`
for breaking changes. The summary becomes a bullet in `CHANGELOG.md` and the
GitHub Release notes.

Merges to `main` run CI first. If changesets are present, the release workflow
bumps the highest requested version, updates `Cargo.toml` and `Cargo.lock`,
removes the consumed changeset files, commits the version and changelog, tags
`v<version>`, builds a Linux x86_64 tarball, and publishes a GitHub Release.
