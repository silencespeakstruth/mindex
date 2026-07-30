// k6 indexing load generator for the mindex perf harness.
//
// Pure HTTP load: it POSTs pre-built corpus shards to /index and reports throughput
// + latency. Everything comes from env vars (set by run.sh); the script never starts
// processes, never touches the OS, and names no hardware.
//
// Each shard is sent exactly once (shared-iterations executor, one iteration per
// shard, fanned across CONCURRENCY VUs) so a run = one full pass over the corpus =
// full embedding work. Run a fresh PROJECT_GUID each time, or the server hash-skips
// unchanged files and the run does no work.
//
// Env:
//   MINDEX_URL    e.g. https://127.0.0.1:11111   (required)
//   PROTOCOL      API version segment            (default: v0)
//   PROJECT_GUID  fresh per run                  (required)
//   CORPUS_DIR    dir holding shard-NNN.json     (required)
//   SHARD_COUNT   number of shards               (required)
//   CONCURRENCY   parallel /index streams (VUs)  (default: 1)
//   MAX_DURATION  whole-scenario safety cap      (default: 1h)
//   REQ_TIMEOUT   per /index request timeout     (default: 600s)
//   INSECURE      skip TLS verify (self-signed)  (default: true)
//   SUMMARY_OUT   file to write the JSON summary (default: stdout only)

import http from 'k6/http';
import exec from 'k6/execution';
import { Counter } from 'k6/metrics';
import { SharedArray } from 'k6/data';
// Native-looking end-of-test summary (k6 fetches + caches this remote module once).
import { textSummary } from 'https://jslib.k6.io/k6-summary/0.1.0/index.js';

const PROTOCOL = __ENV.PROTOCOL || 'v0';
const SHARD_COUNT = Number(__ENV.SHARD_COUNT || 0);
const CONCURRENCY = Number(__ENV.CONCURRENCY || 1);

// Loaded once in init context, shared read-only across all VUs (no per-VU copy).
const shards = new SharedArray('shards', function () {
  const arr = [];
  for (let i = 0; i < SHARD_COUNT; i++) {
    const name = 'shard-' + String(i).padStart(3, '0') + '.json';
    arr.push(open(`${__ENV.CORPUS_DIR}/${name}`));
  }
  return arr;
});

const chunksIndexed = new Counter('chunks_indexed');
const err429 = new Counter('err_429'); // embedder backpressure / claim contention
const err499 = new Counter('err_499'); // client cancelled
const err500 = new Counter('err_500'); // pool exhaustion / internal
const err503 = new Counter('err_503'); // embedder down
const errOther = new Counter('err_other');

export const options = {
  scenarios: {
    index: {
      executor: 'shared-iterations',
      vus: CONCURRENCY,
      iterations: SHARD_COUNT,
      maxDuration: __ENV.MAX_DURATION || '1h',
    },
  },
  insecureSkipTLSVerify: (__ENV.INSECURE || 'true') === 'true',
  summaryTrendStats: ['avg', 'min', 'med', 'p(90)', 'p(95)', 'p(99)', 'max'],
};

export default function () {
  const i = exec.scenario.iterationInTest; // unique 0..SHARD_COUNT-1
  const url = `${__ENV.MINDEX_URL}/${PROTOCOL}/${__ENV.PROJECT_GUID}/index`;
  const res = http.post(url, shards[i], {
    headers: { 'Content-Type': 'application/json' },
    tags: { name: 'index' },
    timeout: __ENV.REQ_TIMEOUT || '600s', // default 60s is far too short for /index
  });

  if (res.status === 200) {
    let n = 0;
    try {
      const files = res.json('files') || {};
      for (const lang in files) {
        for (const path in files[lang]) n += files[lang][path];
      }
    } catch (_) {
      // Non-JSON 200 shouldn't happen; leave chunk count unchanged.
    }
    chunksIndexed.add(n);
  } else if (res.status === 429) {
    err429.add(1);
  } else if (res.status === 499) {
    err499.add(1);
  } else if (res.status === 500) {
    err500.add(1);
  } else if (res.status === 503) {
    err503.add(1);
  } else {
    errOther.add(1);
  }
}

export function handleSummary(data) {
  const m = data.metrics;
  const get = (name, stat) =>
    (m[name] && m[name].values && m[name].values[stat]) || 0;

  const summary = {
    wall_clock_ms: data.state.testRunDurationMs,
    http_reqs: get('http_reqs', 'count'),
    chunks_indexed: get('chunks_indexed', 'count'),
    req_dur_p50: get('http_req_duration', 'med'),
    req_dur_p90: get('http_req_duration', 'p(90)'),
    req_dur_p95: get('http_req_duration', 'p(95)'),
    req_dur_p99: get('http_req_duration', 'p(99)'),
    err_429: get('err_429', 'count'),
    err_499: get('err_499', 'count'),
    err_500: get('err_500', 'count'),
    err_503: get('err_503', 'count'),
    err_other: get('err_other', 'count'),
  };

  // Native metrics table to the terminal; machine JSON to the file run.sh parses.
  const out = { stdout: textSummary(data, { indent: ' ', enableColors: true }) };
  if (__ENV.SUMMARY_OUT) out[__ENV.SUMMARY_OUT] = JSON.stringify(summary);
  return out;
}
