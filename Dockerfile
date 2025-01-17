FROM ubuntu:24.04

RUN apt-get update

#install guacd
RUN apt-get install -y guacd

#install node
RUN apt-get install -y nodejs npm

#copy gclient
RUN mkdir /guacamole-lite
COPY . /guacamole-lite

#install guacamole-lite
RUN cd /guacamole-lite && npm install

COPY docker-entrypoint.sh /usr/local/bin/
RUN chmod +x /usr/local/bin/docker-entrypoint.sh

ENTRYPOINT [ "docker-entrypoint.sh" ]
EXPOSE 8080
ENV GUAC_KEY=secret