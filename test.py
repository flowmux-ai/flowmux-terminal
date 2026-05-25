  import wave, struct
  w=wave.open('/tmp/test.wav','rb'); n=w.getnframes()
  s=struct.unpack('<'+'h'*n,w.readframes(n))
  print(f'peak={max(abs(x) for x in s)/32767:.3f} >10%={sum(1 for x in s if abs(x)>3277)/n*100:.1f}%')

