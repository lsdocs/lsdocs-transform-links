# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0](https://github.com/lsdocs/lsdocs-transform-links/releases/tag/lsdocs-transform-links/v0.1.0) - 2026-11-22

### Added

- release automation ([#3](https://github.com/lsdocs/lsdocs-transform-links/pull/3))
- *(host)* plugin settings in .config/ (Phase 13)
- *(host)* result dependencies and site/parse (Phase 12a)
- content-hash build cache (Phase 7b)
- adopt the shared document shape in plugins (Phase 6d-3 to 6d-5)
- *(host)* normalize parsed documents (Phase 6d-2)
- add site navigation and a sitemap (Phase 6b)
- check links, fill cross-references and add backlinks (Phase 6a)

### Fixed

- settings of the wrong kind are errors; stale options wording
- *(transform-links)* count label links as backlinks

### Other

- *(protocol)* rename pipeline methods (Phase 13)
- deny unwrap, expect and panic outside tests
