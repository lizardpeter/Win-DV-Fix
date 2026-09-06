#!/usr/bin/env python3
from pathlib import Path
import sys


def rep(root, rel, old, new):
    p = root / rel
    s = p.read_text(encoding="utf-8")
    if s.count(old) != 1:
        raise RuntimeError(f"{rel}: expected one match, found {s.count(old)}")
    p.write_text(s.replace(old, new), encoding="utf-8", newline="")


def main():
    root = Path(sys.argv[1]).resolve()

    # Pause resets m_counter in historical WinDV. Verification must depend on
    # actual finalized files, not that resettable UI/session counter.
    rep(root, "DShow.cpp",
        '''\t/* Write capture log CSV if any frames were captured. */\n\tif (m_counter > 0) {\n''',
        '''\t/* Verify/log whenever at least one AVI was actually finalized. */\n\tif (m_finalizedFiles.GetSize() > 0) {\n''')

    # Scanner is disabled in the worker; remove even the 200 ms error-stat UI poll.
    rep(root, "DVToolsDlg.cpp",
'''\tcase CDV::Capturing: {
\t\tErrorStats es = m_video.GetErrorStats();
\t\tif (es.dwFramesWithVideoErrors > 0 && es.dwTotalFrames > 0) {
\t\t\tdouble pct = 100.0 * es.dwFramesWithVideoErrors / es.dwTotalFrames;
\t\t\ttxt3.Format(" Q:%i E:%lu/%.1f%%", m_video.GetQueueLoad(),
\t\t\t\tes.dwFramesWithVideoErrors, pct);
\t\t} else {
\t\t\ttxt3.Format(" Q:%i E:0", m_video.GetQueueLoad());
\t\t}
\t\tbreak;
\t}
''',
'''\tcase CDV::Capturing:
\t\ttxt3.Format(" Q:%i", m_video.GetQueueLoad());
\t\tbreak;
''')

    print("ArchiveSafe final verification fix applied")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
