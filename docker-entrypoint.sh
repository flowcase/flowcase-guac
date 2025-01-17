#!/bin/sh

exec guacd -f -L debug -l 4822 &

cd /guacamole-lite

node server.js $GUAC_KEY