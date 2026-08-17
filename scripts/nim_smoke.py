"""Minimal NVIDIA NIM connectivity probe for the Factory provider.

Usage: NVIDIA_API_KEY=... python scripts/nim_smoke.py "Describe the task"
"""

import os
import sys

import requests


invoke_url = "https://integrate.api.nvidia.com/v1/chat/completions"
stream = False

headers = {
    "Authorization": f"Bearer {os.environ['NVIDIA_API_KEY']}",
    "Accept": "text/event-stream" if stream else "application/json",
}

payload = {
    "model": os.getenv("NIM_MODEL", "stepfun-ai/step-3.7-flash"),
    "messages": [{"role": "user", "content": " ".join(sys.argv[1:]) or "Reply with OK."}],
    "temperature": 1,
    "top_p": 0.95,
    "max_tokens": 16384,
    "seed": 42,
    "stream": stream,
}

response = requests.post(invoke_url, headers=headers, json=payload, stream=stream, timeout=180)
response.raise_for_status()
if stream:
    for line in response.iter_lines():
        if line:
            print(line.decode("utf-8"))
else:
    print(response.json())
