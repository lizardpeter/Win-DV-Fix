#!/usr/bin/env python3
"""Patch WinDV 1.6.0 for explicit, queue-draining End Capture finalization."""
from pathlib import Path
import sys


def replace_once(root: Path, rel: str, old: str, new: str) -> None:
    path = root / rel
    text = path.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise RuntimeError(f"{rel}: expected exactly one match, found {count}\n--- needle ---\n{old}")
    path.write_text(text.replace(old, new), encoding="utf-8", newline="")
    print(f"patched {rel}")


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: windv_end_capture_patch.py <WinDV source root>", file=sys.stderr)
        return 2
    root = Path(sys.argv[1]).resolve()

    # Dedicated UI control.
    replace_once(root, "Resource.h",
        "#define IDC_CHK_SHA256                  1074\n",
        "#define IDC_CHK_SHA256                  1074\n#define IDC_END_CAPTURE                 1075\n")
    replace_once(root, "Resource.h",
        "#define _APS_NEXT_CONTROL_VALUE         1075\n",
        "#define _APS_NEXT_CONTROL_VALUE         1076\n")
    replace_once(root, "WinDV.rc",
        '    PUSHBUTTON      "Record",IDC_RECORD,259,130,50,14,WS_GROUP\n    PUSHBUTTON      "Cancel",IDCANCEL,259,145,50,14,WS_GROUP\n',
        '    PUSHBUTTON      "Record",IDC_RECORD,259,130,50,14,WS_GROUP\n    PUSHBUTTON      "End Capture",IDC_END_CAPTURE,259,145,50,14,WS_GROUP\n    PUSHBUTTON      "Cancel",IDCANCEL,259,145,50,14,WS_GROUP\n')

    # New capture state and synchronous finalizer.
    replace_once(root, "DShow.h",
        "\tenum {Idle, RecordPaused, Recording, CapturePaused, Capturing, Finished} m_state;\n",
        "\tenum {Idle, RecordPaused, Recording, CapturePaused, Capturing, CaptureFinalizing, Finished} m_state;\n")
    replace_once(root, "DShow.h",
        "\t/* Transitions from Capturing to CapturePaused, closing the current AVI file. */\n\tvoid StopCapturing();\n",
        "\t/* Transitions from Capturing to CapturePaused. */\n\tvoid StopCapturing();\n"
        "\t/* Stops frame production, drains already-buffered frames when ending an active\n"
        "\t * capture, waits for CAVIWriter/EOS finalization and post-capture checks. */\n"
        "\tvoid FinalizeCapturing();\n")

    stop_func = """void CDV::StopCapturing()\n{\n\tif (m_state == Capturing) {\n\t\tm_state = CapturePaused;\n\t\tif (m_DVctrl) m_dvInput->CtrlPause();\n\t}\n}\n"""
    finalizer = stop_func + r'''

/*
 * FinalizeCapturing -- deterministic end-of-capture path.
 *
 * When called while actively Capturing, first prevent any new frames from
 * entering the queue, then let CapturingThread drain every frame that was
 * already accepted by WinDV. The queue EOS sentinel is observed only after
 * those buffered frames have been consumed. CapturingThread then destroys
 * CAVIWriter, whose destructor sends EOS to the AVI mux and waits for graph
 * completion before returning. Post-capture AVI/index validation and optional
 * SHA-256 also finish before this method returns.
 *
 * When called from CapturePaused, queued frames are drained without being
 * appended, preserving the user's pause boundary; the open writer is still
 * closed through the same EOS/finalization path.
 */
void CDV::FinalizeCapturing()
{
	if (m_state != Capturing && m_state != CapturePaused)
		return;

	BOOL writeQueuedFrames = (m_state == Capturing);
	m_captureTime = 0;
	m_state = writeQueuedFrames ? CaptureFinalizing : Finished;

	/* Stop tape motion first when WinDV owns transport control, then stop the
	 * DirectShow source so no callback can enqueue a new frame. */
	if (m_DVctrl && m_dvInput) m_dvInput->CtrlPause();
	if (m_dvInput) m_dvInput->Stop();

	/* Queue EOS is ordered after all frames already accepted by Put(). Get()
	 * continues returning buffered frames until the ring is empty, then false. */
	if (m_queue) m_queue->Put(-1, NULL, 0);

	if (m_thread) {
		WaitForSingleObject(m_thread->m_hThread, INFINITE);
		delete m_thread;
		m_thread = NULL;
	}

	m_state = Finished;
}
'''
    replace_once(root, "DShow.cpp", stop_func, finalizer)
    replace_once(root, "DShow.cpp",
        "\t\t\tif (m_state == Capturing) {\n\t\t\t\t/* --- File splitting logic ---\n",
        "\t\t\tif (m_state == Capturing || m_state == CaptureFinalizing) {\n\t\t\t\t/* --- File splitting logic ---\n")

    # Archive-oriented default: signal-loss auto-stop is opt-in, not automatic.
    replace_once(root, "DShow.cpp",
        "  m_autoStopTimeout(5000),\n",
        "  m_autoStopTimeout(0),\n")
    replace_once(root, "DShow.h",
        "\t * 0 = disabled (wait indefinitely). Default: 5000 ms. */\n",
        "\t * 0 = disabled (wait indefinitely). Archive-safe default: disabled. */\n")

    # Do not inherit the normal v1.6 AutoStopTimeout registry value. This keeps
    # a previously saved 5000 ms setting from silently re-enabling auto-stop.
    # The archive build still persists its own opt-in setting separately.
    replace_once(root, "DVToolsDlg.cpp",
        'm_video.m_autoStopTimeout = AfxGetApp()->GetProfileInt("Capture", "AutoStopTimeout", m_video.m_autoStopTimeout);',
        'm_video.m_autoStopTimeout = AfxGetApp()->GetProfileInt("Capture", "ArchiveAutoStopTimeout", m_video.m_autoStopTimeout);')
    replace_once(root, "DVToolsDlg.cpp",
        'AfxGetApp()->WriteProfileInt("Capture", "AutoStopTimeout", m_video.m_autoStopTimeout);',
        'AfxGetApp()->WriteProfileInt("Capture", "ArchiveAutoStopTimeout", m_video.m_autoStopTimeout);')

    # Dialog wiring.
    replace_once(root, "DVToolsDlg.h",
        "\tafx_msg void OnCapture();\n\tafx_msg void OnRecord();\n",
        "\tafx_msg void OnCapture();\n\tafx_msg void OnEndCapture();\n\tafx_msg void OnRecord();\n")
    replace_once(root, "DVToolsDlg.cpp",
        "\t// Action buttons: each visible on its own tab only.\n"
        "\t{IDC_CAPTURE,\t XR, XR,100,100,\tTAB_CAPTURE},\n"
        "\t{IDC_RECORD,\t XR, XR,100,100,\tTAB_RECORD},\n"
        "\t// Cancel button: visible on both active tabs.\n"
        "\t{IDCANCEL,\t\t XR, XR,100,100,\tTAB_CAPTURE | TAB_RECORD},\n",
        "\t// Action buttons: each visible on its own tab only.\n"
        "\t{IDC_CAPTURE,\t XR, XR,100,100,\tTAB_CAPTURE},\n"
        "\t{IDC_RECORD,\t XR, XR,100,100,\tTAB_RECORD},\n"
        "\t// End Capture occupies the lower-right slot on Capture; Cancel remains\n"
        "\t// available on Record only.\n"
        "\t{IDC_END_CAPTURE, XR, XR,100,100,\tTAB_CAPTURE},\n"
        "\t{IDCANCEL,\t\t XR, XR,100,100,\tTAB_RECORD},\n")
    replace_once(root, "DVToolsDlg.cpp",
        "\tON_BN_CLICKED(IDC_CAPTURE, OnCapture)\n\tON_BN_CLICKED(IDC_RECORD, OnRecord)\n",
        "\tON_BN_CLICKED(IDC_CAPTURE, OnCapture)\n\tON_BN_CLICKED(IDC_END_CAPTURE, OnEndCapture)\n\tON_BN_CLICKED(IDC_RECORD, OnRecord)\n")
    replace_once(root, "DVToolsDlg.cpp",
        "/* OnCancel -- suppress the default Escape/Cancel behaviour that would close the\n"
        " * dialog.  Instead, reinitialise the pipeline to the idle state for the active tab.\n"
        " */\nvoid CDVToolsDlg::OnCancel()\n{\n\tInitVideo();\n}\n",
        "/* OnCancel -- suppress the default Escape/Cancel behaviour that would close the\n"
        " * dialog. On Capture, use the same deterministic finalization as End Capture.\n"
        " */\nvoid CDVToolsDlg::OnCancel()\n{\n"
        "\tif (m_toolTab.GetCurSel() == 0 &&\n"
        "\t\t(m_video.GetState() == CDV::Capturing || m_video.GetState() == CDV::CapturePaused)) {\n"
        "\t\tOnEndCapture();\n\t\treturn;\n\t}\n\tInitVideo();\n}\n")
    replace_once(root, "DVToolsDlg.cpp",
        " *   Capturing      -> StopCapturing()   (pause; output file is flushed/closed)\n",
        " *   Capturing      -> StopCapturing()   (pause; End Capture performs finalization)\n")
    replace_once(root, "DVToolsDlg.cpp",
        '"Capturing...  Press <Capture> for pause."',
        '"Capturing... <Capture>=Pause, <End Capture>=Finish safely."')
    replace_once(root, "DVToolsDlg.cpp",
        '"Paused... Press <Capture> for Capturing."',
        '"Paused... <Capture>=Resume, <End Capture>=Finish safely."')

    marker = "\n/* OnRecord -- toggle button handler for the Record tab.\n"
    handler = r'''

/* OnEndCapture -- stop input, drain already-received frames, close the AVI
 * through CAVIWriter EOS, wait for integrity verification, then re-arm capture. */
void CDVToolsDlg::OnEndCapture()
{
	if (m_toolTab.GetCurSel() != 0) return;

	int state = m_video.GetState();
	if (state != CDV::Capturing && state != CDV::CapturePaused) {
		InitVideo();
		return;
	}

	KillTimer(1);
	CWnd *captureButton = GetDlgItem(IDC_CAPTURE);
	CWnd *endButton = GetDlgItem(IDC_END_CAPTURE);
	if (captureButton) captureButton->EnableWindow(FALSE);
	if (endButton) endButton->EnableWindow(FALSE);

	m_status.SetWindowText("Ending capture: draining frames, finalizing AVI, verifying index...");
	UpdateWindow();

	TRY {
		m_video.FinalizeCapturing();
		AVICheckResult r = m_video.GetLastCheckResult();
		BOOL indexOK = r.bValid && r.bHasIndex;

		InitVideo();
		if (indexOK)
			m_status.SetWindowText("Capture ended safely. AVI index verified; ready for next capture.");
		else {
			CString msg;
			msg.Format("Capture ended, but AVI verification reported: %s", (LPCSTR)r.sError);
			m_status.SetWindowText(msg);
			MessageBeep(MB_ICONWARNING);
		}
	}
	CATCH_ALL(e) {
		Exception2Status(e);
	}
	END_CATCH_ALL;

	if (captureButton) captureButton->EnableWindow(TRUE);
	if (endButton) endButton->EnableWindow(TRUE);
}
'''
    replace_once(root, "DVToolsDlg.cpp", marker, handler + marker)

    print("WinDV archival End Capture patch applied successfully")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
