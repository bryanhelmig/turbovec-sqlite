#include <stdio.h>
#include <string.h>

#include <sqlite3.h>
#include "turbovec_sqlite.h"

int main(void) {
    sqlite3 *database = NULL;
    sqlite3_stmt *statement = NULL;

    if (sqlite3_turbovec_auto_extension() != SQLITE_OK ||
        sqlite3_open(":memory:", &database) != SQLITE_OK ||
        sqlite3_prepare_v2(database, "select turbovec_version()", -1, &statement, NULL) != SQLITE_OK ||
        sqlite3_step(statement) != SQLITE_ROW) {
        fprintf(stderr, "static TurboVec registration failed: %s\n", sqlite3_errmsg(database));
        return 1;
    }

    const unsigned char *version = sqlite3_column_text(statement, 0);
    if (version == NULL || strcmp((const char *)version, TURBOVEC_SQLITE_VERSION) != 0) {
        fprintf(stderr, "unexpected TurboVec version\n");
        return 1;
    }

    sqlite3_finalize(statement);
    sqlite3_close(database);
    return 0;
}
