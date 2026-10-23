FROM python:3.13-slim
RUN pip install --no-cache-dir coinbase-advanced-py==1.8.4
WORKDIR /app
COPY src/ /app/
RUN useradd -m -u 1000 worker
USER 1000
ENV PYTHONUNBUFFERED=1 PYTHONDONTWRITEBYTECODE=1
