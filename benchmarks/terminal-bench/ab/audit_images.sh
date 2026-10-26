#!/bin/bash
# Checks every cached Terminal-Bench 2.0 task image for answer material the agent could read:
# solution/test files baked in, and git repositories with history (listed for a manual look).
# Usage: bash benchmarks/terminal-bench/ab/audit_images.sh > audit.txt
# The hosts keep jobs under $HOME/tb2/jobs/ and binaries under $HOME/tb2/bin/; this audit writes
# its report to stdout.
for toml in ~/.cache/harbor/tasks/*/*/task.toml; do
  task=$(basename "$(dirname "$toml")")
  image=$(sed -n 's/^docker_image = "\(.*\)"/\1/p' "$toml")
  [ -n "$image" ] || { echo "$task: no docker_image (built locally)"; continue; }
  docker image inspect "$image" >/dev/null 2>&1 || { echo "$task: $image not pulled"; continue; }
  docker run --rm --network none --entrypoint sh "$image" -c '
    find / -xdev \( -name solve.sh -o -name test_outputs.py -o -path /solution -o -path /tests -o -path /oracle \) 2>/dev/null | head -5 | sed "s/^/  answer-file: /"
    find / -xdev -name .git -type d 2>/dev/null | grep -v -e /usr/ -e /root/.cache -e node_modules | head -5 | while read g; do
      n=$(git -C "${g%/.git}" log --all --oneline 2>/dev/null | wc -l)
      echo "  git: ${g%/.git} commits=$n"
    done' 2>/dev/null | sed "s/^/$task: /"
  echo "$task: checked"
done
