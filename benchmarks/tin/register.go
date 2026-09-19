// Copyright (C) 2026 Ben Weis <ben@springbird.app>
//
// See LICENSE in the repository root for license terms.

package stannum

import (
	"github.com/paradedb/benchmarker/backends"
	pgshared "github.com/paradedb/benchmarker/backends/shared/postgres"
)

func init() {
	backends.Register("stannum", backends.BackendConfig{
		Factory: New, FileType: "sql", EnvVar: "STANNUM_URL",
		DefaultConn: "postgres://postgres:postgres@localhost:35436/benchmark",
		Container:   "stannum",
	})
}

func New(connection string) (backends.Driver, error) {
	driver, err := pgshared.New(connection)
	if err != nil {
		return nil, err
	}
	driver.(*pgshared.Driver).SetIndexIOStatsAccessMethods("stannum")
	return driver, nil
}
