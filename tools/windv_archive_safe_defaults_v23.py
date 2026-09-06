#!/usr/bin/env python3
"""ArchiveSafe v2.3 defaults: no discontinuity split and UINT_MAX AVI frame limit."""
from pathlib import Path
import sys


def rep(root, rel, old, new):
    p = root / rel
    s = p.read_text(encoding="utf-8")
    if s.count(old) != 1:
        raise RuntimeError(f"{rel}: expected one match, found {s.count(old)}\n{old}")
    p.write_text(s.replace(old, new), encoding="utf-8", newline="")
    print("patched", rel)


def main():
    root = Path(sys.argv[1]).resolve()

    # Archive master should remain one file unless an actual external/container
    # limit is reached.  UINT_MAX is the exact type limit of m_maxAVIFrames.
    rep(root, "DShow.cpp",
'''  m_type2AVI(false), m_discontinuityTreshold(1), m_maxAVIFrames(25*60*15), m_everyNth(1), m_recordPreview(TRUE),
''',
'''  m_type2AVI(false), m_discontinuityTreshold(0), m_maxAVIFrames(UINT_MAX), m_everyNth(1), m_recordPreview(TRUE),
''')

    # Do not inherit stock WinDV's saved discontinuity/max-frame values.  Keep
    # ArchiveSafe settings under isolated keys so the new defaults really take
    # effect on systems that have used stock WinDV before.
    rep(root, "DVToolsDlg.cpp",
'''\tm_video.m_discontinuityTreshold = AfxGetApp()->GetProfileInt("Capture", "DiscontinuityTreshold", m_video.m_discontinuityTreshold);
\tm_video.m_maxAVIFrames = AfxGetApp()->GetProfileInt("Capture", "MaxAVIFrames", m_video.m_maxAVIFrames);
''',
'''\tm_video.m_discontinuityTreshold = AfxGetApp()->GetProfileInt("Capture", "ArchiveDiscontinuityThreshold", 0);
\tCString archiveMaxFrames = AfxGetApp()->GetProfileString("Capture", "ArchiveMaxAVIFrames", "");
\tif (archiveMaxFrames.IsEmpty()) {
\t\tm_video.m_maxAVIFrames = UINT_MAX;
\t} else {
\t\tchar *end = NULL;
\t\tunsigned long parsed = strtoul((LPCSTR)archiveMaxFrames, &end, 10);
\t\tm_video.m_maxAVIFrames = (end && *end == '\\0') ? (UINT)parsed : UINT_MAX;
\t}
''')

    # Write UINT_MAX as an unsigned decimal string.  WriteProfileInt takes an
    # int, so using it for 0xffffffff would rely on signed conversion.
    rep(root, "DVToolsDlg.cpp",
'''\tAfxGetApp()->WriteProfileInt("Capture", "DiscontinuityTreshold", m_video.m_discontinuityTreshold);
\tAfxGetApp()->WriteProfileInt("Capture", "MaxAVIFrames", m_video.m_maxAVIFrames);
''',
'''\tAfxGetApp()->WriteProfileInt("Capture", "ArchiveDiscontinuityThreshold", m_video.m_discontinuityTreshold);
\tCString archiveMaxFramesOut;
\tarchiveMaxFramesOut.Format("%u", m_video.m_maxAVIFrames);
\tAfxGetApp()->WriteProfileString("Capture", "ArchiveMaxAVIFrames", archiveMaxFramesOut);
''')

    print("ArchiveSafe v2.3 no-split defaults applied")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
