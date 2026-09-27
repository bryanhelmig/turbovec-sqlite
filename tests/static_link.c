#include <stdio.h>
#include <string.h>

#include <sqlite3.h>
#include "turbovec_sqlite.h"

#define DIMENSIONS 8

static int fail(sqlite3 *database, const char *what) {
    fprintf(stderr, "%s: %s\n", what, sqlite3_errmsg(database));
    return 1;
}

/* A statically linked host reaches the extension through
 * sqlite3_auto_extension(), which hands it SQLite's API routine table. The
 * turbovec0 module reads that table directly for the callbacks Rusqlite does
 * not expose, so checking a scalar function alone would leave the virtual
 * table — the part that can actually break here — untested. */
static int check_virtual_table(sqlite3 *database) {
    sqlite3_stmt *statement = NULL;
    float stored[DIMENSIONS] = {1.0f, 0.0f, 0.0f, 0.0f, 0.0f, 0.0f, 0.0f, 0.0f};
    float other[DIMENSIONS] = {0.0f, 1.0f, 0.0f, 0.0f, 0.0f, 0.0f, 0.0f, 0.0f};

    if (sqlite3_exec(database,
                     "create virtual table v using turbovec0(dimensions=8, bit_width=4)",
                     NULL, NULL, NULL) != SQLITE_OK) {
        return fail(database, "static turbovec0 create failed");
    }

    if (sqlite3_prepare_v2(database, "insert into v(rowid, embedding) values (?, ?)", -1,
                           &statement, NULL) != SQLITE_OK) {
        return fail(database, "static turbovec0 insert prepare failed");
    }
    const float *vectors[2] = {stored, other};
    for (int row = 0; row < 2; row++) {
        sqlite3_reset(statement);
        if (sqlite3_bind_int64(statement, 1, row + 1) != SQLITE_OK ||
            sqlite3_bind_blob(statement, 2, vectors[row], (int)sizeof(stored),
                              SQLITE_STATIC) != SQLITE_OK ||
            sqlite3_step(statement) != SQLITE_DONE) {
            sqlite3_finalize(statement);
            return fail(database, "static turbovec0 insert failed");
        }
    }
    sqlite3_finalize(statement);

    if (sqlite3_prepare_v2(database,
                           "select rowid from v where embedding match ? "
                           "order by score desc limit 1",
                           -1, &statement, NULL) != SQLITE_OK) {
        return fail(database, "static turbovec0 query prepare failed");
    }
    if (sqlite3_bind_blob(statement, 1, other, (int)sizeof(other), SQLITE_STATIC) != SQLITE_OK ||
        sqlite3_step(statement) != SQLITE_ROW) {
        sqlite3_finalize(statement);
        return fail(database, "static turbovec0 query failed");
    }
    sqlite3_int64 nearest = sqlite3_column_int64(statement, 0);
    sqlite3_finalize(statement);
    if (nearest != 2) {
        fprintf(stderr, "static turbovec0 returned rowid %lld, expected 2\n",
                (long long)nearest);
        return 1;
    }

    if (sqlite3_prepare_v2(database, "pragma integrity_check", -1, &statement, NULL) != SQLITE_OK ||
        sqlite3_step(statement) != SQLITE_ROW) {
        sqlite3_finalize(statement);
        return fail(database, "static integrity check failed");
    }
    const unsigned char *report = sqlite3_column_text(statement, 0);
    int healthy = report != NULL && strcmp((const char *)report, "ok") == 0;
    sqlite3_finalize(statement);
    if (!healthy) {
        fprintf(stderr, "static integrity check did not report ok\n");
        return 1;
    }

    /* xIntegrity is filled in by hand on top of Rusqlite's module. SQLite
     * silently reports "ok" for a virtual table that has no xIntegrity, so the
     * only way to prove the static build reaches it is to damage the shadow
     * storage and require the damage back. Do this last: it leaves the table
     * unusable. */
    if (sqlite3_exec(database,
                     "update v_chunks set data=substr(data, 1, length(data)-1) "
                     "where chunk_id=0",
                     NULL, NULL, NULL) != SQLITE_OK) {
        return fail(database, "static shadow-table corruption failed");
    }
    if (sqlite3_prepare_v2(database, "pragma integrity_check", -1, &statement, NULL) != SQLITE_OK ||
        sqlite3_step(statement) != SQLITE_ROW) {
        sqlite3_finalize(statement);
        return fail(database, "static integrity check failed after corruption");
    }
    report = sqlite3_column_text(statement, 0);
    int reported = report != NULL && strstr((const char *)report, "metadata declares") != NULL;
    if (!reported) {
        fprintf(stderr, "static xIntegrity did not report damaged storage: %s\n",
                report == NULL ? "(null)" : (const char *)report);
    }
    sqlite3_finalize(statement);
    return reported ? 0 : 1;
}

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

    if (check_virtual_table(database) != 0) {
        sqlite3_close(database);
        return 1;
    }

    sqlite3_close(database);
    return 0;
}
