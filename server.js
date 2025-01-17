#!/usr/bin/env node

const GuacamoleLite = require('guacamole-lite');
const express = require('express');
const http = require('http');
const { log } = require('console');

const app = express();

const server = http.createServer(app);

const guacdOptions = {
    port: 4822 // port of guacd
};

// Get the key from command line arguments
const args = process.argv.slice(2);
const key = args[0];

const clientOptions = {
    crypt: {
        cypher: 'AES-256-CBC',
        key: key
    }
};

const guacServer = new GuacamoleLite(
    {server},
    guacdOptions,
    clientOptions
);

app.use(express.static('public'));

server.listen(8080);