#!/usr/bin/env node
/** Load turbovec-sqlite with Node's built-in SQLite API. */

import { resolve } from "node:path";
import { DatabaseSync } from "node:sqlite";

if (process.argv.length !== 3) {
  console.error(`usage: ${process.argv[1]} EXTENSION`);
  process.exit(2);
}

const documents = [
  [1, "east", [1, 0, 0, 0, 0, 0, 0, 0]],
  [2, "north", [0, 1, 0, 0, 0, 0, 0, 0]],
  [3, "near east", [0.9, 0.1, 0, 0, 0, 0, 0, 0]],
];

const db = new DatabaseSync(":memory:", { allowExtension: true });
db.loadExtension(resolve(process.argv[2]), "sqlite3_turbovec_init");
db.enableLoadExtension(false);
db.exec(`
  create table documents(id integer primary key, body text not null);
  create virtual table document_vectors using turbovec0(
    dimensions=8,
    bit_width=4
  );
`);

const insertDocument = db.prepare(
  "insert into documents(id, body) values (?, ?)",
);
const insertVector = db.prepare(
  "insert into document_vectors(rowid, embedding) values (?, ?)",
);
db.exec("begin immediate");
for (const [rowid, body, vector] of documents) {
  insertDocument.run(rowid, body);
  insertVector.run(rowid, JSON.stringify(vector));
}
db.exec("commit");

const search = db.prepare(`
  with matches as (
    select rowid, score
    from document_vectors
    where embedding match ?
    order by score desc
    limit ?
  )
  select d.id, d.body, printf('%.4f', matches.score) as score
  from matches
  join documents as d on d.id = matches.rowid
  order by matches.score desc
`);
for (const { id, body, score } of search.all(JSON.stringify(documents[0][2]), 2)) {
  console.log(`${id}\t${body}\t${score}`);
}

db.close();
