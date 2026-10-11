#!/usr/bin/env python3
"""The extension stays loaded after its last connection closes.

SQLite unloads a loadable extension when the last connection that loaded it
closes. A search starts TurboVec's rayon worker threads, which keep running
code from the library. Where unloading unmaps it, as on Windows, those threads
then fault: the model check's reconnects crashed Windows CI this way.
"""

from __future__ import annotations

import ctypes
import os
import sqlite3
import sys
from pathlib import Path


def loaded(extension: Path) -> bool:
    if sys.platform == "win32":
        kernel32 = ctypes.WinDLL("kernel32")
        kernel32.GetModuleHandleW.argtypes = [ctypes.c_wchar_p]
        kernel32.GetModuleHandleW.restype = ctypes.c_void_p
        return bool(kernel32.GetModuleHandleW(extension.name))
    try:
        ctypes.CDLL(str(extension), mode=os.RTLD_NOLOAD)
    except OSError:
        return False
    return True


def main() -> None:
    extension = Path(sys.argv[1]).resolve()
    # Load, search, and close twice: a reload must not undo the first pin.
    for _ in range(2):
        connection = sqlite3.connect(":memory:")
        connection.enable_load_extension(True)
        connection.load_extension(str(extension))
        connection.execute("create virtual table v using turbovec0(dimensions=8,bit_width=4)")
        connection.execute("insert into v(rowid,embedding) values(1,'[1,0,0,0,0,0,0,0]')")
        assert connection.execute(
            "select rowid from v where embedding match '[1,0,0,0,0,0,0,0]' and k=1"
        ).fetchall() == [(1,)]
        connection.close()
    assert loaded(extension), "SQLite unloaded the extension while its worker threads live"
    print("turbovec0 library stays loaded after its last connection closes")


if __name__ == "__main__":
    main()
