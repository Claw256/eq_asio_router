#!/bin/bash
# usage: run2.sh NAME SYNTH SAFETY SECS [extra router-cli args]
name=$1; synth=$2; safety=$3; secs=$4; shift 4
cd /c/Projects/eq_asio_router/measure
EXE=${EXE:-../target/release/router-cli.exe}
rm -rf dump_$name
$EXE run --config "$APPDATA/eq_asio_router/config.toml" --synthetic $synth --safety-ms $safety --preamp-db -20 --seconds $secs --dump-dir dump_$name "$@" > run_$name.log 2>&1
grep -E "^final|^capture:|^master" run_$name.log
for s in capture_in after_resample after_eq; do $EXE analyze dump_$name/$s.wav --skip ${SKIP:-1.5} --tail 0.5 --freq ${FREQ:-0} --channel 0 | grep -vE "^\s+(spike|phase step)"; done
python spec.py dump_$name/after_resample.wav dump_$name/after_eq.wav 2>/dev/null
