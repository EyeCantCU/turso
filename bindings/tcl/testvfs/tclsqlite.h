/* What test_vfs.c needs from SQLite's tclsqlite.h. */
#include <tcl.h>
#ifndef SQLITE_TCLAPI
# define SQLITE_TCLAPI
#endif
#if TCL_MAJOR_VERSION == 9
# define CONST const
#elif !defined(Tcl_Size)
# define Tcl_Size int
#endif
