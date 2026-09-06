WinDV 1.6.0 ArchiveSafe v2.4
Upstream: Wachhund/WinDV f633c456c211a0fb0e05aa43f2b150ec787faab7
Build: Windows x86 Release, Visual Studio 2022
Live DV error scanner: disabled
Archive default: Type-1 DV AVI
Discontinuity threshold default: 0 (automatic discontinuity splitting disabled)
Max AVI frames default: UINT_MAX / 4,294,967,295 frames
Frame decimation: disabled (EveryNth=1)
End Capture: producer stop -> accepted-frame drain -> EOS -> wait for EC_COMPLETE -> AVI finalization -> OpenDML-aware verification
AVI mux finalization: no arbitrary 5-second timeout; fail closed unless EC_COMPLETE is received
AVI verifier: recognizes nested OpenDML indx super-index under hdrl/strl
DirectShow startup: input, preview, and AVI writer accept S_FALSE asynchronous success; only FAILED HRESULTs abort
