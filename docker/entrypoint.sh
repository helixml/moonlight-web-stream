#!/bin/sh
set -e

# Make sure the server folder exists
mkdir -p ${MOONLIGHT_WEB_PATH}/server

# Copy default config if none exists
if [ ! -f /moonlight-web/server/config.json ]; then
    cp ${MOONLIGHT_WEB_PATH}/defaults/config.json /moonlight-web/server/config.json
fi

# Run main application
exec ${MOONLIGHT_WEB_PATH}/web-server