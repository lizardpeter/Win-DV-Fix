#!/usr/bin/env python3
"""Second-stage hardening applied after windv_end_capture_patch.py.

Targets exact WinDV 1.6.0 release source after the ArchiveSafe End Capture
patch. This pass removes hot-path DV error scanning and fixes capture-path
reliability issues found during source audit.
"""
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

    # 1) Never feed uninitialised/bogus DirectShow timestamps into AVI timing.
    rep(root, "DShow.cpp",
'''\tREFERENCE_TIME startTime, endTime;
\tpSample->GetTime(&startTime, &endTime);
\tBYTE *ptr;
\tpSample->GetPointer(&ptr);
\tlong len = pSample->GetActualDataLength();
\t/* Forward the frame duration and raw bytes to the registered handler. */
\t((CInputFilter *)m_pFilter)->m_graph->m_handler->HandleFrame(endTime - startTime, ptr, len);
\treturn hr;
''',
'''\tBYTE *ptr = NULL;
\thr = pSample->GetPointer(&ptr);
\tif (FAILED(hr) || !ptr) return FAILED(hr) ? hr : E_POINTER;
\tlong len = pSample->GetActualDataLength();

\tREFERENCE_TIME startTime = 0, endTime = 0, duration = 0;
\tHRESULT timeHr = pSample->GetTime(&startTime, &endTime);
\tif (SUCCEEDED(timeHr) && endTime > startTime) duration = endTime - startTime;
\t/* Exact nominal DV fallback: PAL 25 fps, NTSC 30000/1001 fps. */
\tif (duration < 300000 || duration > 450000)
\t\tduration = (len >= 144000) ? 400000 : 333667;
\t((CInputFilter *)m_pFilter)->m_graph->m_handler->HandleFrame(duration, ptr, len);
\treturn NOERROR;
''')

    # 2) AVI writer must actually reach Running state before accepting frames.
    rep(root, "DShow.cpp",
'''\t/* Start the graph; frames delivered via HandleFrame() will be written immediately. */
\tm_MC->Run();
}

/*
 * CAVIWriter destructor''',
'''\t/* Start the graph; fail closed if the writer never reaches Running. */
\thr = m_MC->Run();
\tif (hr != S_OK) {
\t\tOAFilterState state;
\t\thr = m_MC->GetState(1000, &state);
\t\tCHECK_HR(hr, "Can't start AVI writer");
\t\tif (state != State_Running)
\t\t\tThrowDShowException(CDShowException::error, "AVI writer did not reach running state");
\t}
}

/*
 * CAVIWriter destructor''')

    # 3) Explicit EOS from CDVQueue must terminate CapturingThread after drain.
    rep(root, "DShow.cpp",
'''\t\tif (!gotFrame && !m_queue->m_end && m_state == Capturing) {
\t\t\t/* Timeout with no EOS: signal lost (tape end or device disconnect). */
\t\t\tcapStats.eStopReason = 1; /* SIGNAL_LOST */
\t\t\tGetParent()->PostMessage(WM_DV_SIGNALLOST, 0, 0);
\t\t\tm_state = Finished;
\t\t\tbreak;
\t\t}
''',
'''\t\tif (!gotFrame && m_queue->m_end) {
\t\t\t/* Producer ended and the ring is empty: all accepted frames are consumed. */
\t\t\tif (m_state != Idle) m_state = Finished;
\t\t\tbreak;
\t\t}
\t\tif (!gotFrame && !m_queue->m_end && m_state == Capturing) {
\t\t\t/* Timeout with no EOS (only when opt-in auto-stop is enabled). */
\t\t\tcapStats.eStopReason = 1; /* SIGNAL_LOST */
\t\t\tGetParent()->PostMessage(WM_DV_SIGNALLOST, 0, 0);
\t\t\tm_state = Finished;
\t\t\tbreak;
\t\t}
''')

    # 4) Disable WDV-10 scanner completely on the per-frame capture path.
    rep(root, "DShow.cpp",
'''\t\t\t/* WDV-10: Analyze the DV frame for error concealment flags (STA). */
\t\t\tFrameErrorInfo frameErrors = AnalyzeDVFrame(buffer, len);
\t\t\t{
\t\t\t\tCAutoLock lock(&m_cs);
\t\t\t\tAccumulateErrorStats(&m_errorStats, &frameErrors, m_errorStats.dwTotalFrames);
\t\t\t}

''',
'''\t\t\t/* ArchiveSafe: live DV DIF error scanner intentionally disabled. */

''')

    # 5) Archive defaults: Type-1 master; never decimate; do not inherit old keys.
    rep(root, "DShow.cpp",
'''  m_type2AVI(true), m_discontinuityTreshold(1), m_maxAVIFrames(25*60*15), m_everyNth(1), m_recordPreview(TRUE),
''',
'''  m_type2AVI(false), m_discontinuityTreshold(1), m_maxAVIFrames(25*60*15), m_everyNth(1), m_recordPreview(TRUE),
''')
    rep(root, "DVToolsDlg.cpp",
'''\tm_video.m_type2AVI = AfxGetApp()->GetProfileInt("Capture", "Type2AVI", m_video.m_type2AVI) > 0;
''',
'''\tm_video.m_type2AVI = AfxGetApp()->GetProfileInt("Capture", "ArchiveType2AVI", m_video.m_type2AVI) > 0;
''')
    rep(root, "DVToolsDlg.cpp",
'''\tm_video.m_everyNth = AfxGetApp()->GetProfileInt("Capture", "EveryNth", m_video.m_everyNth);
''',
'''\tm_video.m_everyNth = 1; /* ArchiveSafe always preserves every received DV frame. */
''')
    rep(root, "DVToolsDlg.cpp",
'''\tAfxGetApp()->WriteProfileInt("Capture", "Type2AVI", m_video.m_type2AVI);
''',
'''\tAfxGetApp()->WriteProfileInt("Capture", "ArchiveType2AVI", m_video.m_type2AVI);
''')
    rep(root, "DVToolsDlg.cpp",
'''\tAfxGetApp()->WriteProfileInt("Capture", "EveryNth", m_video.m_everyNth);
''',
'''\tAfxGetApp()->WriteProfileInt("Capture", "ArchiveEveryNth", 1);
''')
    rep(root, "DVToolsDlg.cpp",
'''\t\tm_video.m_everyNth = captureCfg.m_everyNth;
''',
'''\t\tm_video.m_everyNth = 1; /* ArchiveSafe: frame decimation disabled. */
''')

    # 6) Track exact generated AVI names and verify/hash those, not the UI base name.
    rep(root, "DShow.h",
'''\tCString m_tmpfile, m_filename, m_dtformat;
''',
'''\tCString m_tmpfile, m_filename, m_dtformat, m_finalfile;
''')
    rep(root, "DShow.h",
'''\t/* Sends EOS, waits for graph completion, renames temp file to final path. */
\t~CAVIWriter();
''',
'''\t/* Idempotent finalizer; returns the exact path containing the captured bytes. */
\tCString FinalizeFile();
\t~CAVIWriter();
''')
    rep(root, "DShow.h",
'''\tCString m_captureFilename, m_dtformat;
''',
'''\tCString m_captureFilename, m_dtformat;
\tCArray<CString,CString&> m_finalizedFiles;
''')
    rep(root, "DShow.h",
'''\t/* CFrameHandler implementation: enqueues every incoming frame into m_queue. */
\tvoid HandleFrame(REFERENCE_TIME duration, BYTE *data, int len);
''',
'''\tvoid CloseAVIWriter();
\t/* CFrameHandler implementation: enqueues every incoming frame into m_queue. */
\tvoid HandleFrame(REFERENCE_TIME duration, BYTE *data, int len);
''')

    rep(root, "DShow.cpp",
'''CAVIWriter::~CAVIWriter()
{
\t/* Signal EOS so the AVI mux writes the file index and closes cleanly. */
\tm_outputFilter->m_output->DeliverEndOfStream();
\tif (m_ME) {
\t\tlong evCode;
\t\tm_ME->WaitForCompletion(5000, &evCode);
\t}
\tif (m_MC) m_MC->Stop();
\t/* Compute the final filename and atomically rename from temp.
\t * If MoveFile fails (permissions, disk full), the temp file is kept
\t * so the captured data is not lost. */
\tCString tmp = GetCaptureFilename(m_filename, m_dtformat, m_ndigits, m_dvtime);
\tif (!MoveFile(m_tmpfile, tmp)) {
\t\tTRACE("CAVIWriter: MoveFile(\\\"%s\\\", \\\"%s\\\") failed, error %d\\n",
\t\t\t  (LPCSTR)m_tmpfile, (LPCSTR)tmp, GetLastError());
\t}
}
''',
'''CString CAVIWriter::FinalizeFile()
{
\tif (!m_finalfile.IsEmpty()) return m_finalfile;
\tm_outputFilter->m_output->DeliverEndOfStream();
\tif (m_ME) {
\t\tlong evCode;
\t\tm_ME->WaitForCompletion(5000, &evCode);
\t}
\tif (m_MC) m_MC->Stop();
\tCString finalPath = GetCaptureFilename(m_filename, m_dtformat, m_ndigits, m_dvtime);
\tif (MoveFile(m_tmpfile, finalPath)) m_finalfile = finalPath;
\telse {
\t\tTRACE("CAVIWriter: MoveFile(\\\"%s\\\", \\\"%s\\\") failed, error %d\\n",
\t\t\t  (LPCSTR)m_tmpfile, (LPCSTR)finalPath, GetLastError());
\t\tm_finalfile = m_tmpfile; /* preserve and verify the actual temp file */
\t}
\treturn m_finalfile;
}

CAVIWriter::~CAVIWriter()
{
\tFinalizeFile();
}
''')

    rep(root, "DShow.cpp",
'''void CDV::BuildCapturing(LPCSTR vsrc)
{
\tDestroy();
\tHRESULT hr = S_OK;
''',
'''void CDV::BuildCapturing(LPCSTR vsrc)
{
\tDestroy();
\tm_finalizedFiles.RemoveAll();
\tHRESULT hr = S_OK;
''')

    marker = '''\n\n/*\n * CDV::CapturingThread\n'''
    helper = '''\n\nvoid CDV::CloseAVIWriter()\n{\n\tif (!m_aviWriter) return;\n\tCString p = m_aviWriter->FinalizeFile();\n\tif (!p.IsEmpty()) m_finalizedFiles.Add(p);\n\tdelete m_aviWriter;\n\tm_aviWriter = NULL;\n}\n'''
    rep(root, "DShow.cpp", marker, helper + marker)

    p = root / "DShow.cpp"
    s = p.read_text(encoding="utf-8")
    old = '''\t\t\t\t\tdelete m_aviWriter;\n\t\t\t\t\tm_aviWriter = NULL;\n'''
    if s.count(old) < 2:
        raise RuntimeError("writer delete blocks not found")
    s = s.replace(old, '''\t\t\t\t\tCloseAVIWriter();\n''')
    s = s.replace('''\tif (m_aviWriter) {\n\t\tdelete m_aviWriter;\n\t\tm_aviWriter = NULL;\n\t}\n\t/* Write capture log CSV if any frames were captured. */''',
                  '''\tif (m_aviWriter) CloseAVIWriter();\n\t/* Write capture log CSV if any frames were captured. */''')
    s = s.replace('''\tif (m_aviWriter) { delete(m_aviWriter);\tm_aviWriter = NULL; }\n''',
                  '''\tif (m_aviWriter) CloseAVIWriter();\n''')
    p.write_text(s, encoding="utf-8", newline="")

    rep(root, "DShow.cpp",
'''\t\t/* Run AVI integrity check on the finished file. */
\t\tm_lastCheckResult = CheckAVIIntegrity(m_captureFilename);
\t\tcapStats.bCheckPassed = m_lastCheckResult.bValid
\t\t\t&& m_lastCheckResult.dwDefectFrames == 0
\t\t\t&& m_lastCheckResult.bHasIndex;
\t\tcapStats.dwCheckDefect = m_lastCheckResult.dwDefectFrames;
\t\tcapStats.bCheckIndex = m_lastCheckResult.bHasIndex;

\t\t/* WDV-11: Compute SHA-256 of the finalized AVI file and write sidecar. */
\t\tif (m_enableSHA256) {
\t\t\tif (ComputeFileSHA256(m_captureFilename, capStats.szSHA256)) {
\t\t\t\t/* Extract bare filename for the sidecar line. */
\t\t\t\tCString bareName = m_captureFilename;
\t\t\t\tint sep = bareName.ReverseFind('\\\\');
\t\t\t\tif (sep >= 0) bareName = bareName.Mid(sep + 1);

\t\t\t\t/* Write sidecar next to the capture file. */
\t\t\t\tCString sidecarPath;
\t\t\t\tsidecarPath.Format("%s.sha256", (LPCSTR)m_captureFilename);
\t\t\t\tWriteSHA256Sidecar(sidecarPath, capStats.szSHA256, bareName, FALSE);
\t\t\t}
\t\t}
''',
'''\t\t/* Verify every exact path actually finalized by CAVIWriter. */
\t\tcapStats.bCheckPassed = (m_finalizedFiles.GetSize() > 0);
\t\tcapStats.bCheckIndex = (m_finalizedFiles.GetSize() > 0);
\t\tcapStats.dwCheckDefect = 0;
\t\tBOOL haveFailure = FALSE;
\t\tfor (int i = 0; i < m_finalizedFiles.GetSize(); ++i) {
\t\t\tCString f = m_finalizedFiles[i];
\t\t\tAVICheckResult r = CheckAVIIntegrity(f);
\t\t\tBOOL ok = r.bValid && r.dwDefectFrames == 0 && r.bHasIndex;
\t\t\tif (!ok) {
\t\t\t\tcapStats.bCheckPassed = FALSE;
\t\t\t\tif (!haveFailure) { m_lastCheckResult = r; haveFailure = TRUE; }
\t\t\t} else if (!haveFailure) m_lastCheckResult = r;
\t\t\tif (!r.bHasIndex) capStats.bCheckIndex = FALSE;
\t\t\tcapStats.dwCheckDefect += r.dwDefectFrames;
\t\t\tif (m_enableSHA256) {
\t\t\t\tchar hash[65];
\t\t\t\tif (ComputeFileSHA256(f, hash)) {
\t\t\t\t\tCString bare = f; int sep = bare.ReverseFind('\\\\');
\t\t\t\t\tif (sep >= 0) bare = bare.Mid(sep + 1);
\t\t\t\t\tCString side; side.Format("%s.sha256", (LPCSTR)f);
\t\t\t\t\tWriteSHA256Sidecar(side, hash, bare, FALSE);
\t\t\t\t\tif (m_finalizedFiles.GetSize() == 1)
\t\t\t\t\t\tlstrcpyn(capStats.szSHA256, hash, sizeof capStats.szSHA256);
\t\t\t\t}
\t\t\t}
\t\t}
\t\tif (m_finalizedFiles.GetSize() == 1) capStats.sFilename = m_finalizedFiles[0];
''')

    print("ArchiveSafe hardening applied")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
