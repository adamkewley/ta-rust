#!/usr/bin/env bash

echo "Hi, I'm a basic bash script"

sleep 0.3

echo "Let me just go ahead an expose my internals ;)"
set -xe

echo "Ah, much better"

echo "Type the correct guess"
read var

while [[ "${var}" != 'the correct guess' ]]; do
      echo "Your answer '${var}' was the wrong guess, try again!"
      read var
done

sleep 0.3

echo "DING DING DING! That was the correct guess!"

sleep 0.3


sleep 1
