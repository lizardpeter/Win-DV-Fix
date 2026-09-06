# Type-1 vs Type-2 DV AVI

## Recommendation

For preservation capture, use **Type-1 DV AVI** as the master.

DV camcorders transmit interleaved DV frames in which the audio data is already carried with the video frame. Type-1 stores that native interleaved DV stream as one AVI stream. Type-2 runs the same DV data through a DV Splitter and additionally stores separate audio stream data for compatibility with older editing software.

Microsoft's DirectShow documentation explicitly recommends Type-1 for video capture when maximum throughput matters because Type-2 adds redundant audio storage and processor work.

References:
- https://learn.microsoft.com/en-us/windows/win32/directshow/type-1-vs--type-2-dv-avi-files
- https://learn.microsoft.com/en-us/windows/win32/directshow/dv-data-in-the-avi-file-format
- https://learn.microsoft.com/en-us/windows/win32/directshow/dv-splitter-filter

## Quality

A correctly produced Type-1 and Type-2 file can contain the same DV video essence. Type-1 is not lower quality and Type-2 is not higher quality. The difference is container/stream organization.

## Conversion later

A Type-1 master is not a dead end. It can later be converted to Type-2 using a DV splitter, and its audio can be extracted/decoded to PCM/WAV. The original DV video does not need to be re-encoded for these derivative operations.

Recommended archival workflow:

1. Capture the tape once to Type-1 DV AVI.
2. Verify and hash that file.
3. Keep that Type-1 AVI unchanged as the preservation master.
4. Generate Type-2 AVI, WAV/PCM audio, editing proxies, or modern distribution files from the master as needed.

## Additional Type-2 caveat

Microsoft documents a Type-2-specific edge case for tapes containing heterogeneous sources: if the DV audio format changes, the downstream AVI Mux can reject the format change and the DV Splitter can stop producing its separate audio stream. This does not affect Type-1 capture because the native interleaved DV stream is not split during capture.
