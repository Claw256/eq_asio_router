import numpy as np,sys
def rd(p,ch=0):
    b=open(p,'rb').read()
    rate=int.from_bytes(b[24:28],'little')
    d=np.frombuffer(b[44:],dtype='<f4').reshape(-1,2)[:,ch].astype(float)
    return rate,d
if __name__=='__main__':
    rate,d=rd(sys.argv[1])
    a=int(float(sys.argv[2])*rate); n=int(float(sys.argv[3])*rate) if len(sys.argv)>3 else 2*rate
    x=d[a:a+n]
    f0=1000.0
    t=np.arange(len(x))/rate
    z=x*np.exp(-2j*np.pi*f0*t)
    L=48
    k=np.ones(L)/L
    zz=np.convolve(z,k,'valid')
    ph=np.unwrap(np.angle(zz)); amp=np.abs(zz)*2
    fi=np.diff(ph)*rate/(2*np.pi)+f0
    # print per 512 block mean inst freq & amp stats
    B=512
    for i in range(0,min(len(fi),B*40),B):
        print(i//B, 'f=%.3f'%fi[i:i+B].mean(), 'fstd=%.3f'%fi[i:i+B].std(), 'amp=%.5f..%.5f'%(amp[i:i+B].min(),amp[i:i+B].max()))
