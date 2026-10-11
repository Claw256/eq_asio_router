import numpy as np,sys
from an import rd
def spec(path,a,b,f0=1000.0,label=''):
    rate,d=rd(path); x=d[int(a*rate):int(b*rate)]
    w=np.hanning(len(x)); X=np.abs(np.fft.rfft(x*w))**2; f=np.fft.rfftfreq(len(x),1/rate)
    tot=X.sum(); car=X[(abs(f-f0)<3)].sum()
    near=X[(abs(f-f0)>=3)&(abs(f-f0)<100)].sum(); far=X[(abs(f-f0)>=100)].sum()
    print('%s: carrier=1.0 near sidebands(3-100Hz) %.1f dB, far(>100Hz) %.1f dB'%(label,10*np.log10(near/car+1e-30),10*np.log10(far/car+1e-30)))
for p in sys.argv[1:]:
    spec(p,7.5,11.5,label=p)
