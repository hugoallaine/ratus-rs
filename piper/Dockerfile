# Serveur de voix Piper (HTTP, POST /synthesize). ~160 Mo de RAM avec la voix upmc.
FROM python:3.13-slim-trixie
ARG VOICE=fr_FR-upmc-medium
ARG VOICE_PATH=fr/fr_FR/upmc/medium
RUN pip install --no-cache-dir "piper-tts[http]==1.8.0" \
 && python -c "import urllib.request as u; [u.urlretrieve(f'https://huggingface.co/rhasspy/piper-voices/resolve/main/${VOICE_PATH}/${VOICE}.{e}', f'/voice.{e}') for e in ('onnx', 'onnx.json')]"
USER nobody
EXPOSE 5000
CMD ["python", "-m", "piper.http_server", "-m", "/voice.onnx", "--host", "0.0.0.0", "--port", "5000"]
