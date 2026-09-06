#!/usr/bin/env python3
"""ArchiveSafe v2.5: truthful live End Capture phase and SHA-256 progress UI."""
from pathlib import Path
import sys


def rep(root, rel, old, new):
    p = root / rel
    s = p.read_text(encoding="utf-8")
    count = s.count(old)
    if count != 1:
        raise RuntimeError(f"{rel}: expected one match, found {count}\n--- needle ---\n{old}")
    p.write_text(s.replace(old, new), encoding="utf-8", newline="")
    print("patched", rel)


def main():
    if len(sys.argv) != 2:
        print("usage: windv_archive_safe_progress_v25.py <WinDV source root>", file=sys.stderr)
        return 2
    root = Path(sys.argv[1]).resolve()

    # A dedicated synchronous progress message. FinalizeCapturing is called on
    # the UI thread, so its short worker-wait polling loop can SendMessage this
    # safely without any worker->UI blocking cycle.
    rep(root, "DShow.h",
'''#define WM_DV_CHECK_COMPLETE\t(WM_USER+204)\n''',
'''#define WM_DV_CHECK_COMPLETE\t(WM_USER+204)\n\n/* Sent synchronously by FinalizeCapturing while it polls the capture worker.\n * wParam = CDV::Finalize* phase, lParam = percentage for hash phase. */\n#define WM_DV_FINALIZE_PROGRESS\t(WM_USER+205)\n''')

    rep(root, "DShow.h",
'''\tenum {Idle, RecordPaused, Recording, CapturePaused, Capturing, CaptureFinalizing, Finished} m_state;\n''',
'''\tenum {Idle, RecordPaused, Recording, CapturePaused, Capturing, CaptureFinalizing, Finished} m_state;\n\t/* End Capture progress is written only by the capture worker/finalizer and\n\t * read by the UI thread. LONG stores are atomic on supported Win32 targets. */\n\tenum {FinalizeNone, FinalizeDraining, FinalizeMux, FinalizeVerify, FinalizeHash, FinalizeDone};\n\tvolatile LONG m_finalizePhase;\n\tvolatile LONG m_finalizePercent;\n''')

    rep(root, "DShow.h",
'''\t/* WDV-10: Returns a snapshot of the current DV error statistics (thread-safe). */\n\tErrorStats GetErrorStats();\n''',
'''\t/* WDV-10: Returns a snapshot of the current DV error statistics (thread-safe). */\n\tErrorStats GetErrorStats();\n\tLONG GetFinalizePhase() const { return m_finalizePhase; }\n\tLONG GetFinalizePercent() const { return m_finalizePercent; }\n''')

    rep(root, "DShow.h",
'''BOOL ComputeFileSHA256(LPCSTR szFilePath, char szHashOut[65]);\n''',
'''BOOL ComputeFileSHA256(LPCSTR szFilePath, char szHashOut[65], volatile LONG *pProgressPercent = NULL);\n''')

    # SHA progress is based on bytes actually read from the finalized file.
    rep(root, "DShow.cpp",
'''BOOL ComputeFileSHA256(LPCSTR szFilePath, char szHashOut[65])\n{\n\tszHashOut[0] = '\\0';\n\n\tHANDLE hFile = CreateFile(szFilePath, GENERIC_READ, FILE_SHARE_READ,\n\t\tNULL, OPEN_EXISTING, FILE_FLAG_SEQUENTIAL_SCAN, NULL);\n\tif (hFile == INVALID_HANDLE_VALUE)\n\t\treturn FALSE;\n\n\tSHA256_CTX ctx;\n\tsha256_init(&ctx);\n\n\tBYTE buf[65536];\n\tDWORD dwRead;\n\twhile (ReadFile(hFile, buf, sizeof buf, &dwRead, NULL) && dwRead > 0) {\n\t\tsha256_update(&ctx, buf, dwRead);\n\t}\n\n\tCloseHandle(hFile);\n\n\tunsigned char hash[32];\n\tsha256_final(&ctx, hash);\n\tsha256_hex(hash, szHashOut);\n\n\treturn TRUE;\n}\n''',
'''BOOL ComputeFileSHA256(LPCSTR szFilePath, char szHashOut[65], volatile LONG *pProgressPercent)\n{\n\tszHashOut[0] = '\\0';\n\tif (pProgressPercent) InterlockedExchange(pProgressPercent, 0);\n\n\tHANDLE hFile = CreateFile(szFilePath, GENERIC_READ, FILE_SHARE_READ,\n\t\tNULL, OPEN_EXISTING, FILE_FLAG_SEQUENTIAL_SCAN, NULL);\n\tif (hFile == INVALID_HANDLE_VALUE)\n\t\treturn FALSE;\n\n\tLARGE_INTEGER fileSize;\n\tfileSize.QuadPart = 0;\n\tGetFileSizeEx(hFile, &fileSize);\n\tULONGLONG bytesReadTotal = 0;\n\tLONG lastPercent = -1;\n\n\tSHA256_CTX ctx;\n\tsha256_init(&ctx);\n\n\tBYTE buf[65536];\n\tDWORD dwRead;\n\tBOOL readOK = TRUE;\n\tfor (;;) {\n\t\tif (!ReadFile(hFile, buf, sizeof buf, &dwRead, NULL)) { readOK = FALSE; break; }\n\t\tif (dwRead == 0) break;\n\t\tsha256_update(&ctx, buf, dwRead);\n\t\tbytesReadTotal += dwRead;\n\t\tif (pProgressPercent && fileSize.QuadPart > 0) {\n\t\t\tLONG pct = (LONG)((bytesReadTotal * 100ULL) / (ULONGLONG)fileSize.QuadPart);\n\t\t\tif (pct > 100) pct = 100;\n\t\t\tif (pct != lastPercent) {\n\t\t\t\tInterlockedExchange(pProgressPercent, pct);\n\t\t\t\tlastPercent = pct;\n\t\t\t}\n\t\t}\n\t}\n\n\tCloseHandle(hFile);\n\tif (!readOK) {\n\t\tif (pProgressPercent) InterlockedExchange(pProgressPercent, 0);\n\t\treturn FALSE;\n\t}\n\n\tunsigned char hash[32];\n\tsha256_final(&ctx, hash);\n\tsha256_hex(hash, szHashOut);\n\tif (pProgressPercent) InterlockedExchange(pProgressPercent, 100);\n\n\treturn TRUE;\n}\n''')

    # Initialize/reset progress only outside the live frame path.
    rep(root, "DShow.cpp",
'''  m_autoStopTimeout(0),\n  m_enableSHA256(true)\n''',
'''  m_autoStopTimeout(0),\n  m_enableSHA256(true),\n  m_finalizePhase(FinalizeNone), m_finalizePercent(0)\n''')

    rep(root, "DShow.cpp",
'''\tDestroy();\n\tm_finalizedFiles.RemoveAll();\n\tHRESULT hr = S_OK;\n''',
'''\tDestroy();\n\tm_finalizedFiles.RemoveAll();\n\tInterlockedExchange(&m_finalizePhase, FinalizeNone);\n\tInterlockedExchange(&m_finalizePercent, 0);\n\tHRESULT hr = S_OK;\n''')

    # FinalizeCapturing polls rather than blocking forever in one call. This does
    # not change what the worker does; it only lets the UI thread repaint real
    # phase/progress values while preserving the same complete join semantics.
    rep(root, "DShow.cpp",
'''\tBOOL writeQueuedFrames = (m_state == Capturing);\n\tm_captureTime = 0;\n\tm_state = writeQueuedFrames ? CaptureFinalizing : Finished;\n''',
'''\tBOOL writeQueuedFrames = (m_state == Capturing);\n\tm_captureTime = 0;\n\tInterlockedExchange(&m_finalizePhase, FinalizeDraining);\n\tInterlockedExchange(&m_finalizePercent, 0);\n\tm_state = writeQueuedFrames ? CaptureFinalizing : Finished;\n''')

    rep(root, "DShow.cpp",
'''\tif (m_thread) {\n\t\tWaitForSingleObject(m_thread->m_hThread, INFINITE);\n\t\tdelete m_thread;\n\t\tm_thread = NULL;\n\t}\n\n\tm_state = Finished;\n}\n\n/*\n * CDV::StartCapturing\n''',
'''\tif (m_thread) {\n\t\tLONG lastPhase = -1, lastPercent = -1;\n\t\tfor (;;) {\n\t\t\tDWORD wait = WaitForSingleObject(m_thread->m_hThread, 100);\n\t\t\tLONG phase = m_finalizePhase;\n\t\t\tLONG percent = m_finalizePercent;\n\t\t\tif (phase != lastPhase || percent != lastPercent) {\n\t\t\t\tCWnd *parent = GetParent();\n\t\t\t\tif (parent && parent->GetSafeHwnd())\n\t\t\t\t\tparent->SendMessage(WM_DV_FINALIZE_PROGRESS, (WPARAM)phase, (LPARAM)percent);\n\t\t\t\tlastPhase = phase;\n\t\t\t\tlastPercent = percent;\n\t\t\t}\n\t\t\tif (wait == WAIT_OBJECT_0) break;\n\t\t\tif (wait == WAIT_FAILED) {\n\t\t\t\t/* Preserve capture safety if the timed wait itself fails. */\n\t\t\t\tWaitForSingleObject(m_thread->m_hThread, INFINITE);\n\t\t\t\tbreak;\n\t\t\t}\n\t\t}\n\t\tdelete m_thread;\n\t\tm_thread = NULL;\n\t}\n\n\tInterlockedExchange(&m_finalizePhase, FinalizeDone);\n\tInterlockedExchange(&m_finalizePercent, 100);\n\tCWnd *parent = GetParent();\n\tif (parent && parent->GetSafeHwnd())\n\t\tparent->SendMessage(WM_DV_FINALIZE_PROGRESS, FinalizeDone, 100);\n\tm_state = Finished;\n}\n\n/*\n * CDV::StartCapturing\n''')

    # Worker-owned phase transitions. CloseAVIWriter may also be used for a
    # normal split/pause, so only advertise mux finalization during End Capture.
    rep(root, "DShow.cpp",
'''void CDV::CloseAVIWriter()\n{\n\tif (!m_aviWriter) return;\n\tCString p = m_aviWriter->FinalizeFile();\n''',
'''void CDV::CloseAVIWriter()\n{\n\tif (!m_aviWriter) return;\n\tif (m_finalizePhase != FinalizeNone)\n\t\tInterlockedExchange(&m_finalizePhase, FinalizeMux);\n\tCString p = m_aviWriter->FinalizeFile();\n''')

    rep(root, "DShow.cpp",
'''\t\t/* Verify every exact path actually finalized by CAVIWriter. */\n\t\tcapStats.bCheckPassed = (m_finalizedFiles.GetSize() > 0);\n''',
'''\t\t/* Verify every exact path actually finalized by CAVIWriter. */\n\t\tif (m_finalizePhase != FinalizeNone) {\n\t\t\tInterlockedExchange(&m_finalizePhase, FinalizeVerify);\n\t\t\tInterlockedExchange(&m_finalizePercent, 0);\n\t\t}\n\t\tcapStats.bCheckPassed = (m_finalizedFiles.GetSize() > 0);\n''')

    rep(root, "DShow.cpp",
'''\t\t\tif (m_enableSHA256) {\n\t\t\t\tchar hash[65];\n\t\t\t\tif (ComputeFileSHA256(f, hash)) {\n''',
'''\t\t\tif (m_enableSHA256) {\n\t\t\t\tInterlockedExchange(&m_finalizePhase, FinalizeHash);\n\t\t\t\tInterlockedExchange(&m_finalizePercent, 0);\n\t\t\t\tchar hash[65];\n\t\t\t\tif (ComputeFileSHA256(f, hash, &m_finalizePercent)) {\n''')

    rep(root, "DShow.cpp",
'''\t\t/* Notify the UI about the check result. */\n\t\tGetParent()->PostMessage(WM_DV_CHECK_COMPLETE,\n''',
'''\t\tInterlockedExchange(&m_finalizePhase, FinalizeDone);\n\t\tInterlockedExchange(&m_finalizePercent, 100);\n\n\t\t/* Notify the UI about the check result. */\n\t\tGetParent()->PostMessage(WM_DV_CHECK_COMPLETE,\n''')

    # Dialog progress handler and sticky final-result status.
    rep(root, "DVToolsDlg.h",
'''\t/* OnDVCheckComplete -- handles WM_DV_CHECK_COMPLETE posted after a\n\t * post-capture AVI integrity check. wParam = bValid (1=OK, 0=errors). */\n\tafx_msg LRESULT OnDVCheckComplete(WPARAM, LPARAM);\n''',
'''\t/* OnDVCheckComplete -- handles WM_DV_CHECK_COMPLETE posted after a\n\t * post-capture AVI integrity check. wParam = bValid (1=OK, 0=errors). */\n\tafx_msg LRESULT OnDVCheckComplete(WPARAM, LPARAM);\n\t/* Live End Capture phase/progress, sent synchronously from FinalizeCapturing. */\n\tafx_msg LRESULT OnDVFinalizeProgress(WPARAM, LPARAM);\n''')

    rep(root, "DVToolsDlg.h",
'''\tbool m_exitOnFinish;\n''',
'''\tbool m_exitOnFinish;\n\t/* Keeps the completed End Capture result visible after InitVideo re-arms\n\t * the capture graph instead of letting the 200 ms timer replace it with Paused. */\n\tbool m_captureFinalizedStatusSticky;\n''')

    rep(root, "DVToolsDlg.cpp",
'''\tON_MESSAGE(WM_DV_CHECK_COMPLETE, OnDVCheckComplete)\nEND_MESSAGE_MAP()\n''',
'''\tON_MESSAGE(WM_DV_CHECK_COMPLETE, OnDVCheckComplete)\n\tON_MESSAGE(WM_DV_FINALIZE_PROGRESS, OnDVFinalizeProgress)\nEND_MESSAGE_MAP()\n''')

    rep(root, "DVToolsDlg.cpp",
'''\tm_minWidth = m_minHeight = 1;\n\tm_exitOnFinish = 0;\n}\n''',
'''\tm_minWidth = m_minHeight = 1;\n\tm_exitOnFinish = 0;\n\tm_captureFinalizedStatusSticky = false;\n}\n''')

    rep(root, "DVToolsDlg.cpp",
'''void CDVToolsDlg::InitVideo()\n{\n\tm_exitOnFinish = 0;\n''',
'''void CDVToolsDlg::InitVideo()\n{\n\tm_exitOnFinish = 0;\n\tm_captureFinalizedStatusSticky = false;\n''')

    rep(root, "DVToolsDlg.cpp",
'''void CDVToolsDlg::OnCapture()\n{\n\tif (m_video.GetState() == CDV::CapturePaused) {\n''',
'''void CDVToolsDlg::OnCapture()\n{\n\tm_captureFinalizedStatusSticky = false;\n\tif (m_video.GetState() == CDV::CapturePaused) {\n''')

    rep(root, "DVToolsDlg.cpp",
'''\tm_status.SetWindowText("Ending capture: draining frames, finalizing AVI, verifying index...");\n\tUpdateWindow();\n''',
'''\tm_captureFinalizedStatusSticky = false;\n\tm_status.SetWindowText("Ending capture - draining queued DV frames...");\n\tUpdateWindow();\n''')

    rep(root, "DVToolsDlg.cpp",
'''\t\tInitVideo();\n\t\tif (indexOK)\n\t\t\tm_status.SetWindowText("Capture ended safely. AVI index verified; ready for next capture.");\n\t\telse {\n''',
'''\t\tInitVideo();\n\t\tm_captureFinalizedStatusSticky = true;\n\t\tif (indexOK) {\n\t\t\tif (m_video.m_enableSHA256)\n\t\t\t\tm_status.SetWindowText("Capture safely finalized. AVI/OpenDML index verified; SHA-256 complete; ready for next capture.");\n\t\t\telse\n\t\t\t\tm_status.SetWindowText("Capture safely finalized. AVI/OpenDML index verified; ready for next capture.");\n\t\t}\n\t\telse {\n''')

    # The timer may continue running after InitVideo re-arms the input graph.
    # Preserve the explicit final result until the next real user action/reset.
    rep(root, "DVToolsDlg.cpp",
'''\tm_status.GetWindowText(tmp);\n\tif (tmp != txt) m_status.SetWindowText(txt);\n''',
'''\tm_status.GetWindowText(tmp);\n\tif (!(m_captureFinalizedStatusSticky && m_video.GetState() == CDV::CapturePaused) && tmp != txt)\n\t\tm_status.SetWindowText(txt);\n''')

    # A WM_DV_CHECK_COMPLETE posted by the worker can arrive after OnEndCapture
    # has already installed the richer sticky result; do not overwrite it.
    rep(root, "DVToolsDlg.cpp",
'''LRESULT CDVToolsDlg::OnDVCheckComplete(WPARAM wParam, LPARAM)\n{\n\tconst AVICheckResult& r = m_video.GetLastCheckResult();\n''',
'''LRESULT CDVToolsDlg::OnDVCheckComplete(WPARAM wParam, LPARAM)\n{\n\tif (m_captureFinalizedStatusSticky) return 0;\n\tconst AVICheckResult& r = m_video.GetLastCheckResult();\n''')

    marker = '''\nLRESULT CDVToolsDlg::OnDVCheckComplete(WPARAM wParam, LPARAM)\n'''
    handler = r'''

/* Live, truthful End Capture status. lParam is meaningful only for SHA phase. */
LRESULT CDVToolsDlg::OnDVFinalizeProgress(WPARAM wParam, LPARAM lParam)
{
	CString msg;
	switch ((LONG)wParam) {
	case CDV::FinalizeDraining:
		msg = "Ending capture - draining queued DV frames...";
		break;
	case CDV::FinalizeMux:
		msg = "Finalizing AVI - waiting for AVI mux/index completion...";
		break;
	case CDV::FinalizeVerify:
		msg = "AVI finalized - verifying RIFF/OpenDML index...";
		break;
	case CDV::FinalizeHash: {
		LONG pct = (LONG)lParam;
		if (pct < 0) pct = 0;
		if (pct > 100) pct = 100;
		msg.Format("AVI verification complete - calculating SHA-256... %ld%%", pct);
		break;
	}
	case CDV::FinalizeDone:
		msg = "Post-capture checks complete - preparing next capture...";
		break;
	default:
		return 0;
	}
	m_status.SetWindowText(msg);
	m_status.UpdateWindow();
	return 0;
}
'''
    rep(root, "DVToolsDlg.cpp", marker, handler + marker)

    print("ArchiveSafe v2.5 live finalization progress applied")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
