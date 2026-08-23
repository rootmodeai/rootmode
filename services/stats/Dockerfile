FROM python:3.12-slim

# Built from the repository root, because the image carries the site as well
# as the service:
#   docker build -f services/stats/Dockerfile -t rootmode-stats .
WORKDIR /srv
COPY services/stats/requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt

COPY services/stats/app ./app
# The site travels with the collector, so one container serves both and the
# explorer reads its numbers from its own origin.
COPY apps/web /srv/web
ENV ROOTMODE_WEB_DIR=/srv/web

# The database and the optional GeoLite2 file live on a volume: losing either
# is losing the history the explorer draws.
VOLUME /var/lib/rootmode
ENV ROOTMODE_STATS_DB=/var/lib/rootmode/stats.db

EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s \
  CMD python -c "import urllib.request;urllib.request.urlopen('http://127.0.0.1:8080/healthz')"

CMD ["uvicorn", "app.main:app", "--host", "0.0.0.0", "--port", "8080"]
