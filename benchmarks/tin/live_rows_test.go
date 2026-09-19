// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

package postgres

import (
	"context"
	"os"
	"strings"
	"testing"

	"github.com/jackc/pgx/v5"
)

// Exercise the production SQL against real empty heap pages, not a mocked row
// count. Only the random page expression is replaced, to make both paths exact.
func TestLiveTupleFallback(t *testing.T) {
	url := os.Getenv("STANNUM_BENCH_TEST_URL")
	if url == "" {
		t.Skip("set STANNUM_BENCH_TEST_URL to an isolated benchmark server")
	}
	ctx := context.Background()
	conn, err := pgx.Connect(ctx, url)
	if err != nil {
		t.Fatal(err)
	}
	defer conn.Close(ctx)
	_, err = conn.Exec(ctx, `CREATE TEMP TABLE live_page_probe(id integer, body text);
		ALTER TABLE live_page_probe ALTER COLUMN body SET STORAGE PLAIN;`)
	if err != nil {
		t.Fatal(err)
	}
	for _, trial := range []struct {
		block string
		id    int
	}{{"1", 3}, {"3", 1}} {
		_, err = conn.Exec(ctx, `TRUNCATE live_page_probe;
			INSERT INTO live_page_probe SELECT n, repeat('x',6000) FROM generate_series(1,4) n;
			DELETE FROM live_page_probe WHERE id IN (2,4);`)
		if err != nil {
			t.Fatal(err)
		}
		query := strings.Replace(appendSpaceToRandomDocumentSQL("live_page_probe"),
			"floor(random() * blocks)::bigint", trial.block+"::bigint", 1)
		var actual int
		err := conn.QueryRow(ctx, query+" RETURNING id", "live_page_probe").Scan(&actual)
		if err != nil {
			t.Fatalf("empty page %s: %v", trial.block, err)
		}
		if actual != trial.id {
			t.Fatalf("empty page %s updated %d, want %d", trial.block, actual, trial.id)
		}
	}
	_, err = conn.Exec(ctx, "DELETE FROM live_page_probe")
	if err != nil {
		t.Fatal(err)
	}
	tag, err := conn.Exec(ctx, appendSpaceToRandomDocumentSQL("live_page_probe"), "live_page_probe")
	if err != nil || tag.RowsAffected() != 0 {
		t.Fatalf("empty relation: %v, %v", tag, err)
	}
}
