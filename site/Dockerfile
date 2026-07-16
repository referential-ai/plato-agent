FROM nginx:1.30.4-alpine3.24@sha256:59d10bca5c674965ef4ff884715000dd60ef5567c36663523f108eec8e4105d4

RUN rm -rf /etc/nginx/conf.d/default.conf /usr/share/nginx/html/*

COPY site/nginx.conf /etc/nginx/nginx.conf
COPY site/index.html /usr/share/nginx/html/index.html
COPY site/blog/ /usr/share/nginx/html/blog/
COPY site/assets/ /usr/share/nginx/html/assets/
COPY site/feed.xml /usr/share/nginx/html/feed.xml
COPY site/sitemap.xml /usr/share/nginx/html/sitemap.xml
COPY site/robots.txt /usr/share/nginx/html/robots.txt
COPY docs/images/desktop-plato-agent.png /usr/share/nginx/html/assets/plato-agent-desktop.png

USER nginx
EXPOSE 8080
