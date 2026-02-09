# Changelog

All notable changes are documented in this file.
The sections should follow the order `Packaging`, `Added`, `Changed`, `Fixed` and `Removed`.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).

## Unreleased

### Fixed

- Crash on startup if `~/.cache/share/pinax` is missing

## 1.2.2 - 2026-02-08

### Fixed

- Crashing on startup due to improper Wayland initialization order

## 1.2.1 - 2025-12-16

### Fixed

- Crash when moving the cursor into preedit text
- Redundant redraws on IME input

## 1.2.0 - 2025-10-03

### Fixed

- Blurry text on initial render
- Text not always saving on exit
- Excessive IME updates
- Inconsistent selection caret grabbing with single character selections

## 1.1.0 - 2025-07-24

### Added

- Logo icon

## 1.0.1 - 2025-07-13

### Fixed

- Touch position not matching cursor offset
- Crash with unicode characters

## 1.0.0 - 2025-07-12

Initial Release.
