#!/usr/bin/env python3
"""ArchiveSafe v2.4: wait for real AVI mux completion and verify nested OpenDML index."""
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

    # A five-second timeout is not a valid archival finalization policy for a
    # large AVI.  After EOS the writer must be allowed to reach EC_COMPLETE.
    # Fail closed on an actual graph error/user abort instead of renaming and
    # hashing a file whose mux/index finalization did not complete.
    rep(root, "DShow.cpp",
'''\tif (!m_finalfile.IsEmpty()) return m_finalfile;
\tm_outputFilter->m_output->DeliverEndOfStream();
\tif (m_ME) {
\t\tlong evCode;
\t\tm_ME->WaitForCompletion(5000, &evCode);
\t}
\tif (m_MC) m_MC->Stop();
''',
'''\tif (!m_finalfile.IsEmpty()) return m_finalfile;

\tHRESULT eosHr = m_outputFilter->m_output->DeliverEndOfStream();
\tif (FAILED(eosHr)) {
\t\tif (m_MC) m_MC->Stop();
\t\tm_finalfile = m_tmpfile;
\t\tThrowDShowException(CDShowException::error, "AVI finalization failed while sending end-of-stream");
\t}

\tif (m_ME) {
\t\tlong evCode = 0;
\t\t/* ArchiveSafe: do not truncate AVI mux/index finalization at an arbitrary
\t\t * timeout.  Once the producer is stopped and EOS has been delivered,
\t\t * EC_COMPLETE is the authoritative indication that the mux has finished. */
\t\tHRESULT waitHr = m_ME->WaitForCompletion(INFINITE, &evCode);
\t\tif (FAILED(waitHr) || evCode != EC_COMPLETE) {
\t\t\tif (m_MC) m_MC->Stop();
\t\t\tm_finalfile = m_tmpfile;
\t\t\tThrowDShowException(CDShowException::error, "AVI mux did not complete finalization");
\t\t}
\t}
\tif (m_MC) m_MC->Stop();
''')

    # The AVI 2.0/OpenDML super-index ('indx') normally lives inside the
    # stream-list hierarchy (hdrl -> strl), not necessarily at RIFF top level.
    # The old checker skipped the whole hdrl LIST after reading avih, so it
    # could miss a valid hierarchical index.  Scan only metadata lists; never
    # walk the multi-gigabyte movi payload.
    marker = '''/* ---- Main implementation ---- */\n\nAVICheckResult CheckAVIIntegrity(LPCSTR szPath)\n'''
    helper = r'''/* Search AVI metadata LISTs for an OpenDML super-index without scanning movi data. */
static BOOL ContainsOpenDMLIndex(HANDLE hFile, __int64 start, __int64 end, int depth)
{
	if (depth < 0) return FALSE;
	__int64 pos = start;
	while (pos + 8 <= end) {
		if (!SeekAbs(hFile, pos)) return FALSE;
		RIFFChunk ck;
		if (!ReadExact(hFile, &ck, 8)) return FALSE;

		__int64 payload = pos + 8;
		__int64 nextPos = payload + (__int64)PADEVEN(ck.dwSize);
		if (nextPos <= pos || nextPos > end) return FALSE;

		if (FCC(ck.fourcc, "indx")) return TRUE;

		if (FCC(ck.fourcc, "LIST") && ck.dwSize >= 4) {
			char listType[4];
			if (!ReadExact(hFile, listType, 4)) return FALSE;
			if ((FCC(listType, "hdrl") || FCC(listType, "strl")) &&
			    ContainsOpenDMLIndex(hFile, payload + 4,
			                           pos + 8 + (__int64)ck.dwSize, depth - 1))
				return TRUE;
		}

		pos = nextPos;
	}
	return FALSE;
}

/* ---- Main implementation ---- */

AVICheckResult CheckAVIIntegrity(LPCSTR szPath)
'''
    rep(root, "AVICheck.cpp", marker, helper)

    rep(root, "AVICheck.cpp",
'''\t\t\tif (FCC(listType, "hdrl")) {
\t\t\t\tbHasHdrl = TRUE;
\t\t\t\t/* First sub-chunk of hdrl should be avih. */
\t\t\t\tRIFFChunk avihCk;
\t\t\t\tif (ReadExact(hFile, &avihCk, 8) && FCC(avihCk.fourcc, "avih")) {
\t\t\t\t\tDWORD readSz = avihCk.dwSize;
\t\t\t\t\tif (readSz > sizeof(AVIMainHeader))
\t\t\t\t\t\treadSz = sizeof(AVIMainHeader);
\t\t\t\t\tif (ReadExact(hFile, &avih, readSz))
\t\t\t\t\t\tbHasAvih = TRUE;
\t\t\t\t}
\t\t\t}
''',
'''\t\t\tif (FCC(listType, "hdrl")) {
\t\t\t\tbHasHdrl = TRUE;
\t\t\t\t/* First sub-chunk of hdrl should be avih. */
\t\t\t\tRIFFChunk avihCk;
\t\t\t\tif (ReadExact(hFile, &avihCk, 8) && FCC(avihCk.fourcc, "avih")) {
\t\t\t\t\tDWORD readSz = avihCk.dwSize;
\t\t\t\t\tif (readSz > sizeof(AVIMainHeader))
\t\t\t\t\t\treadSz = sizeof(AVIMainHeader);
\t\t\t\t\tif (ReadExact(hFile, &avih, readSz))
\t\t\t\t\t\tbHasAvih = TRUE;
\t\t\t\t}
\t\t\t\t/* AVI 2.0 super-index is normally nested under hdrl/strl. */
\t\t\t\tif (ContainsOpenDMLIndex(hFile, pos + 12,
\t\t\t\t                           pos + 8 + (__int64)ck.dwSize, 3))
\t\t\t\t\tr.bHasIndex = TRUE;
\t\t\t}
''')

    print("ArchiveSafe v2.4 finalization/OpenDML fixes applied")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
