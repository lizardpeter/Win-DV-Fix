# Win-DV-Fix

Archive-focused WinDV build for reliable lossless DV tape capture.

This repository is based on the exact `Wachhund/WinDV` v1.6.0 commit:

`f633c456c211a0fb0e05aa43f2b150ec787faab7`

The build keeps WinDV's raw DV capture path while hardening capture finalization and removing unnecessary work from the live capture loop.

## ArchiveSafe changes

- Adds an explicit **End Capture** path: stop the producer, drain DV frames already accepted into WinDV, send EOS, wait for AVI mux/index finalization, then verify the finalized file.
- Fixes queue EOS termination so the worker cannot spin after an orderly end of stream.
- Disables the live DV DIF error scanner and its UI polling during capture.
- Defaults to **Type-1 DV AVI** for archival masters; Type-2 remains selectable.
- Locks frame decimation to `EveryNth=1` so WinDV never intentionally discards received DV frames.
- Disables signal-loss auto-stop by default and isolates the ArchiveSafe setting from older WinDV profiles.
- Validates DirectShow sample timing and uses nominal PAL/NTSC DV frame duration only when timestamps are missing or invalid.
- Verifies that the AVI writer reaches the Running state before capture continues.
- Tracks the exact generated AVI filename(s), including split/suffixed files, for verification and optional SHA-256.
- Keeps Pause -> End Capture verification correct even though historical WinDV resets its UI frame counter when paused.

## Type-1 vs Type-2

For a preservation master, this project defaults to **Type-1**. A DV frame already carries synchronized video and audio. Type-1 writes that native interleaved DV stream directly into AVI, avoiding the extra DV Splitter stage used for Type-2.

There is no inherent picture-quality advantage to Type-1 over a correctly produced Type-2 file: both can preserve the same DV essence. The Type-1 recommendation is about keeping the capture path as direct and preservation-oriented as possible.

A Type-1 master can later be converted to Type-2, remuxed, or have its audio decoded/extracted to PCM/WAV without recapturing the tape. Keep the original Type-1 AVI as the immutable master and create editing/audio derivatives from it.

## Repository layout

- `src/` - complete patched WinDV source generated from the pinned upstream commit.
- `tools/` - deterministic ArchiveSafe patch scripts.
- `dist/` - built Win32/x86 release executable and SHA-256.
- `.github/workflows/build.yml` - reproducible Windows build and verification workflow.

The bootstrap workflow imports the temporary patch scripts from the earlier development branch once, generates the complete self-contained source tree and executable here, and commits them into this repository. After that, this repository is the canonical home for the work.
