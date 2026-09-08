"""Reproduce the production statistics SQL's scaling on an indexed SQLite fixture.
Run from the repository root. This extracts the current query instead of copying it.
"""
import json
import pathlib
import sqlite3
import time
import hashlib

source = pathlib.Path('src/extras/js/skills/operations.rs').read_text()
# The query lives in a named const so both this probe and the Rust plan
# regression measure exactly what production runs.
query = source.split('const SKILL_STATS_SQL: &str = "', 1)[1].split('";\n', 1)[0]

def measure(size):
    db = sqlite3.connect(':memory:')
    db.executescript('''
    CREATE TABLE skill_revisions (id TEXT PRIMARY KEY, status TEXT, identity_version INTEGER, capability_json TEXT);
    CREATE TABLE skill_stats (skill_id TEXT PRIMARY KEY, invoked_count INTEGER, direct_success_count INTEGER, direct_failure_count INTEGER, user_positive_count INTEGER, user_negative_count INTEGER);
    CREATE TABLE skill_events (event_id INTEGER PRIMARY KEY, skill_id TEXT, turn_id TEXT, invocation_id TEXT, event_kind TEXT, production INTEGER DEFAULT 1, evidence_complete INTEGER DEFAULT 1, created_at INTEGER);
    CREATE INDEX skill_events_skill_time_idx ON skill_events(skill_id,created_at,event_id);
    CREATE INDEX skill_events_turn_idx ON skill_events(skill_id,turn_id,invocation_id);
    CREATE TABLE skill_task_outcomes (evidence_id TEXT PRIMARY KEY, turn_id TEXT, verify_passed INTEGER, attempt INTEGER, source_kind TEXT, source_id TEXT, production INTEGER, evidence_complete INTEGER DEFAULT 1, created_at INTEGER);
    CREATE TABLE skill_task_outcome_links (evidence_id TEXT, skill_id TEXT, PRIMARY KEY(evidence_id,skill_id));
    CREATE INDEX skill_task_outcomes_source_idx ON skill_task_outcomes(source_kind,source_id,production,created_at);
    CREATE INDEX skill_task_outcome_links_skill_idx ON skill_task_outcome_links(skill_id,evidence_id);
    ''')
    for n in range(size):
        skill_id = f'{n:064x}'
        db.execute('INSERT INTO skill_revisions VALUES (?,\'active\',2,\'{}\')', (skill_id,))
        for kind in ('with', 'without'):
            identity = f'{kind}-{n}'
            db.execute('INSERT INTO skill_task_outcomes VALUES (?,?,1,1,\'oracle\',\'same-oracle\',1,1,1)', (identity,identity))
        db.execute('INSERT INTO skill_task_outcome_links VALUES (?,?)', (f'with-{n}',skill_id))
    db.commit()
    steps = 0
    def progress():
        nonlocal steps
        steps += 100
        return int(steps >= 50_000_000)
    db.set_progress_handler(progress, 100)
    started = time.perf_counter()
    rows = db.execute(query).fetchall()
    elapsed = time.perf_counter() - started
    db.set_progress_handler(None, 0)
    assert len(rows) == size
    assert all(row[6:10] == (1,1,size,size) for row in rows)
    plan = [r[3] for r in db.execute('EXPLAIN QUERY PLAN '+query)]
    db.close()
    return {'revisions':size,'outcomes':size*2,'vm_steps_rounded_down_100':steps,'elapsed_seconds':elapsed,'query_plan':plan}

samples=[measure(n) for n in (20,40,80)]
report={'sqlite_version':sqlite3.sqlite_version,'query_sha256':hashlib.sha256(query.encode()).hexdigest(),'scope':'exact production SQL, reduced fixture schema with relevant production indexes; system SQLite, not bundled Rust SQLite','samples':samples,'doubling_step_ratios':[samples[i+1]['vm_steps_rounded_down_100']/samples[i]['vm_steps_rounded_down_100'] for i in range(2)]}
print(json.dumps(report,indent=2))
