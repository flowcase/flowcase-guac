FROM ubuntu:22.04

RUN apt-get update

#install guacd
RUN apt-get install -y guacd

#install node
RUN apt-get install -y nodejs

#copy gclient
RUN mkdir /guacamole-lite
COPY . /guacamole-lite

COPY docker-entrypoint.sh /usr/local/bin/
RUN chmod +x /usr/local/bin/docker-entrypoint.sh

ENTRYPOINT [ "docker-entrypoint.sh" ]
EXPOSE 8080
ENV GUAC_KEY=secret