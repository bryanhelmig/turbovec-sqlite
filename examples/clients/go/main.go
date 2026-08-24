package main

import (
	"database/sql"
	"encoding/json"
	"flag"
	"fmt"
	"log"
	"path/filepath"

	"github.com/mattn/go-sqlite3"
)

type document struct {
	id     int
	body   string
	path   string
	vector []float32
}

func main() {
	extensionFlag := flag.String("extension", "", "path to the turbovec-sqlite library")
	flag.Parse()
	if *extensionFlag == "" {
		log.Fatal("usage: go run . -extension PATH")
	}
	extension, err := filepath.Abs(*extensionFlag)
	if err != nil {
		log.Fatal(err)
	}

	sql.Register("sqlite3_turbovec", &sqlite3.SQLiteDriver{
		// database/sql may open more than one connection. ConnectHook loads the
		// extension into every connection rather than only the first one.
		ConnectHook: func(connection *sqlite3.SQLiteConn) error {
			return connection.LoadExtension(extension, "sqlite3_turbovec_init")
		},
	})
	db, err := sql.Open("sqlite3_turbovec", ":memory:")
	if err != nil {
		log.Fatal(err)
	}
	defer db.Close()
	db.SetMaxOpenConns(1)

	_, err = db.Exec(`
		create table documents(
			id integer primary key,
			body text not null,
			path text not null
		);
		create virtual table document_vectors using turbovec0(
			dimensions=8,
			bit_width=4
		);
	`)
	if err != nil {
		log.Fatal(err)
	}

	documents := []document{
		{1, "east", "guides/east.yaml", []float32{1, 0, 0, 0, 0, 0, 0, 0}},
		{2, "north", "notes/north.txt", []float32{0, 1, 0, 0, 0, 0, 0, 0}},
		{3, "near east", "guides/near-east.yaml", []float32{0.9, 0.1, 0, 0, 0, 0, 0, 0}},
	}
	tx, err := db.Begin()
	if err != nil {
		log.Fatal(err)
	}
	for _, item := range documents {
		vector, err := json.Marshal(item.vector)
		if err != nil {
			log.Fatal(err)
		}
		if _, err := tx.Exec(
			"insert into documents(id, body, path) values (?, ?, ?)",
			item.id,
			item.body,
			item.path,
		); err != nil {
			log.Fatal(err)
		}
		if _, err := tx.Exec(
			// A string binds as JSON text; []byte would bind as a float32 BLOB.
			"insert into document_vectors(rowid, embedding) values (?, ?)", item.id, string(vector),
		); err != nil {
			log.Fatal(err)
		}
	}
	if err := tx.Commit(); err != nil {
		log.Fatal(err)
	}

	query, err := json.Marshal(documents[0].vector)
	if err != nil {
		log.Fatal(err)
	}
	rows, err := db.Query(`
		with matches as (
			select rowid, score
			from document_vectors
			where embedding match ?
			  and rowid in (
				select id from documents where path glob ?
			  )
			order by score desc
			limit ?
		)
		select d.id, d.body, printf('%.4f', matches.score)
		from matches
		join documents as d on d.id = matches.rowid
		order by matches.score desc
	`, string(query), "*.yaml", 2)
	if err != nil {
		log.Fatal(err)
	}
	defer rows.Close()
	for rows.Next() {
		var id int
		var body, score string
		if err := rows.Scan(&id, &body, &score); err != nil {
			log.Fatal(err)
		}
		fmt.Printf("%d\t%s\t%s\n", id, body, score)
	}
	if err := rows.Err(); err != nil {
		log.Fatal(err)
	}
}
