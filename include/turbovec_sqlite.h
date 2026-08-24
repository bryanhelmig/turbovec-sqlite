#ifndef TURBOVEC_SQLITE_H
#define TURBOVEC_SQLITE_H

#include <sqlite3.h>

typedef struct sqlite3_api_routines sqlite3_api_routines;

#ifdef __cplusplus
extern "C" {
#endif

int sqlite3_turbovec_init(
    sqlite3 *database,
    char **error_message,
    sqlite3_api_routines *api
);

/* Call once before opening connections in a statically linked host. */
static inline int sqlite3_turbovec_auto_extension(void) {
    return sqlite3_auto_extension((void (*)(void))sqlite3_turbovec_init);
}

#ifdef __cplusplus
}
#endif

#endif
