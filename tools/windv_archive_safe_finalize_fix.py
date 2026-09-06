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

    # IMediaControl::Run() may return S_FALSE while a graph is successfully
    # transitioning asynchronously. Stock WinDV allowed this. Reject only
    # FAILED HRESULTs for the live input, preview push source, and AVI writer.
    rep(root, "DShow.cpp",
'''\tm_handler = handler;
\tHRESULT hr = m_MC->Run();
\tCHECK_HR(hr, "Can't start input graph");
''',
'''\tm_handler = handler;
\tHRESULT hr = m_MC->Run();
\tif (FAILED(hr))
\t\tThrowDShowException(CDShowException::error, "Can't start input graph");
''')

    rep(root, "DShow.cpp",
'''\thr = m_MC->Run();
\tCHECK_HR(hr, "Can't start preview graph");

\t/* Start the monitoring thread suspended so we can clear m_bAutoDelete first. */
''',
'''\thr = m_MC->Run();
\tif (FAILED(hr))
\t\tThrowDShowException(CDShowException::error, "Can't start preview graph");

\t/* Start the monitoring thread suspended so we can clear m_bAutoDelete first. */
''')

    rep(root, "DShow.cpp",
'''\t/* Start the graph; fail closed if the writer never reaches Running. */
\thr = m_MC->Run();
\tif (hr != S_OK) {
\t\tOAFilterState state;
\t\thr = m_MC->GetState(1000, &state);
\t\tCHECK_HR(hr, "Can't start AVI writer");
\t\tif (state != State_Running)
\t\t\tThrowDShowException(CDShowException::error, "AVI writer did not reach running state");
\t}
''',
'''\t/* Push-source writer may return S_FALSE until its first sample arrives. */
\thr = m_MC->Run();
\tif (FAILED(hr))
\t\tThrowDShowException(CDShowException::error, "Can't start AVI writer");
''')

    print("ArchiveSafe final verification + asynchronous graph startup fixes applied")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
