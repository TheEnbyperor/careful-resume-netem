#!/usr/bin/env bash

docker build -t "theenbyperor/careful-resume-netem-server:latest" containers -f containers/Dockerfile.server || exit
docker build -t "theenbyperor/careful-resume-netem-client:latest" containers -f containers/Dockerfile.client || exit
