# Browser suite for the soak. The base image carries the browsers; the pip
# package must be the same version or it looks for browsers that are not there.
FROM mcr.microsoft.com/playwright/python:v1.55.0-noble
RUN pip install --no-cache-dir --break-system-packages playwright==1.55.0 argon2-cffi==25.1.0
WORKDIR /pw
COPY suite.py /pw/suite.py
CMD ["python", "/pw/suite.py"]
