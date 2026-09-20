// Copyright (C) 2026 Ben Weis <ben@springbird.app>
// See LICENSE in the repository root for license terms.

package dashboard

import (
	k6metrics "go.k6.io/k6/metrics"
	"testing"
	"time"
)

func TestStannumSlowSamplesExtendExportAfterIdleTimeout(t *testing.T) {
	registry := k6metrics.NewRegistry()
	started, _ := registry.NewMetric("scenario_started", k6metrics.Gauge)
	duration, _ := registry.NewMetric("query_duration", k6metrics.Trend)
	tags := registry.RootTagSet().With("backend", "postgres").With("scenario", "postgres")
	o := &Output{data: &DashboardData{StartTime: time.UnixMilli(1000), Runs: map[string]*RunMetrics{}, Containers: map[string]*ContainerMetrics{}}}
	push := func(metric *k6metrics.Metric, at int64) {
		o.AddMetricSamples([]k6metrics.SampleContainer{k6metrics.Samples{{TimeSeries: k6metrics.TimeSeries{Metric: metric, Tags: tags}, Time: time.UnixMilli(at), Value: 1}}})
		o.flush()
	}
	push(started, 1000)
	push(duration, 1268)
	if o.data.Runs["postgres"].EndTime != 1268 {
		t.Fatal("fixture did not trigger idle timeout")
	}
	push(duration, 5000)
	push(duration, 600950)
	push(duration, 4000) // Late delivery must not shrink the measurement window.
	run := o.getExportData()["runs"].(map[string]interface{})["postgres"].(map[string]interface{})
	if run["startTime"] != int64(1000) || run["endTime"] != int64(600950) {
		t.Fatalf("exported window = %v .. %v, want 1000 .. 600950", run["startTime"], run["endTime"])
	}
}
