import math, sys
LAB_TO_LMS=[[1,0.3963377774,0.2158037573],[1,-0.1055613458,-0.0638541728],[1,-0.0894841775,-1.2914855480]]
LMS_TO_RGB=[[4.0767416621,-3.3077115913,0.2309699292],[-1.2684380046,2.6097574011,-0.3413193965],[-0.0041960863,-0.7034186147,1.7076147010]]
K1,K2=0.206,0.03; K3=(1+K1)/(1+K2)
def mul(m,v): return [sum(m[i][j]*v[j] for j in range(3)) for i in range(3)]
def lin2s(v): return 1.055*v**(1/2.4)-0.055 if v>0.0031308 else 12.92*v
def lab2lin(lab): return mul(LMS_TO_RGB,[x**3 for x in mul(LAB_TO_LMS,lab)])
def lin2rgb(l): return tuple(int(round(min(max(lin2s(v),0),1)*255)) for v in l)
def slopes(a,b): return [LAB_TO_LMS[i][1]*a+LAB_TO_LMS[i][2]*b for i in range(3)]
FIT=[([-1.88170328,-0.80936493],[1.19086277,1.76576728,0.59662641,0.75515197,0.56771245]),
     ([1.81444104,-1.19445276],[0.73956515,-0.45954404,0.08285427,0.12541073,-0.14503204]),
     ([0.13110758,1.81333971],[1.35733652,-0.00915799,-1.15130210,-0.50559606,0.00692167])]
def max_sat(a,b):
    ch=2
    for i,(hp,_) in enumerate(FIT):
        if hp[0]*a+hp[1]*b>1: ch=i; break
    k0,k1,k2,k3,k4=FIT[ch][1]; w=LMS_TO_RGB[ch]
    S=k0+k1*a+k2*b+k3*a*a+k4*a*b; sl=slopes(a,b); base=[1+S*k for k in sl]
    dot=lambda v: sum(w[i]*v[i] for i in range(3))
    f=dot([x**3 for x in base]); f1=dot([3*sl[i]*base[i]**2 for i in range(3)]); f2=dot([6*sl[i]**2*base[i] for i in range(3)])
    return S-(f*f1)/(f1*f1-0.5*f*f2)
def cusp(a,b):
    S=max_sat(a,b); lin=lab2lin([1,S*a,S*b]); L=(1/max(lin))**(1/3); return (L,L*S)
def max_chroma(a,b,L,cu):
    cl,cc=cu
    if L<=cl: return cc*L/cl
    t=cc*(L-1)/(cl-1); sl=slopes(a,b); lms=[L+t*k for k in sl]
    cubes=[x**3 for x in lms]; first=[3*sl[i]*lms[i]**2 for i in range(3)]; second=[6*sl[i]**2*lms[i] for i in range(3)]
    steps=[]
    for row in LMS_TO_RGB:
        dot=lambda v: sum(row[i]*v[i] for i in range(3))
        f=dot(cubes)-1; f1=dot(first); f2=dot(second); u=f1/(f1*f1-0.5*f*f2)
        steps.append(-f*u if u>=0 else float('inf'))
    return t+min(steps)
def stops(L,a,b):
    pk=cusp(a,b); cmax=max_chroma(a,b,L,pk)
    k=cmax/min(L*(pk[1]/pk[0]),(1-L)*(pk[1]/(1-pk[0])))
    ms=0.11516993+1/(7.44778970+4.15901240*b+a*(-2.19557347+1.75198401*b+a*(-2.13704948-10.02301043*b+a*(-4.24894561+5.38770819*b+4.69891013*a))))
    mt=0.11239642+1/(1.61320320-0.68124379*b+a*(0.40370612+0.90148123*b+a*(-0.27087943+0.61223990*b+a*(0.00299215-0.45399568*b-0.14661872*a))))
    cmid=0.9*k*math.sqrt(math.sqrt(1/(1/(L*ms)**4+1/((1-L)*mt)**4)))
    c0=math.sqrt(1/(1/(L*0.4)**2+1/((1-L)*0.8)**2))
    return c0,cmid,cmax
def okhsl(h,s,l):
    L=(l*l+K1*l)/(K3*(l+K2)); lab=[L,0,0]
    if 0<L<1 and s>0:
        ang=2*math.pi*(h%360)/360; a,b=math.cos(ang),math.sin(ang); c0,cmid,cmax=stops(L,a,b)
        if s<0.8:
            t=1.25*s; k1=0.8*c0; C=(t*k1)/(1-(1-k1/cmid)*t)
        else:
            t=5*(s-0.8); k1=(0.2*cmid*cmid*1.25*1.25)/c0; C=cmid+(t*k1)/(1-(1-k1/(cmax-cmid))*t)
        lab=[L,C*a,C*b]
    return lin2rgb(lab2lin(lab))
def parse(s):
    h,sp,lp=s[6:-1].split(); return okhsl(float(h),float(sp[:-1])/100,float(lp[:-1])/100)
if __name__=='__main__':
    tests={'okhsl(250 60% 55%)':(78,136,194),'okhsl(30 100% 50%)':(222,17,0),'okhsl(0 0% 0%)':(0,0,0),'okhsl(0 0% 100%)':(255,255,255),'okhsl(145 78% 61%)':(87,169,92),'okhsl(200 50% 25%)':(29,64,66)}
    for k,v in tests.items(): print(k, parse(k), v, 'OK' if parse(k)==v else 'MISMATCH')
    pal={'TEXT':'okhsl(234 3% 89%)','MUTED':'okhsl(229 6% 67%)','DIM':'okhsl(229 8% 56%)','VIOLET':'okhsl(295 50% 67%)','BLUE':'okhsl(232 54% 67%)','SYNTAX_VARIABLE':'okhsl(202 58% 67%)','SYNTAX_STRING':'okhsl(52 67% 67%)','GREEN':'okhsl(159 59% 67%)','RED':'okhsl(20 72% 67%)','YELLOW':'okhsl(83 88% 67%)','THINKING':'okhsl(226 7% 65%)','BORDER_ACCENT':'okhsl(295 53% 64%)','USER_BG':'okhsl(233 41% 24%)','TOOL_PENDING_BG':'okhsl(229 5% 24%)','TOOL_SUCCESS_BG':'okhsl(158 46% 25%)','TOOL_ERROR_BG':'okhsl(19 54% 25%)','SELECTION_BG':'okhsl(233 41% 34%)'}
    for k,v in pal.items():
        r,g,b=parse(v); print(f'{k}={r};{g};{b}  #{r:02x}{g:02x}{b:02x}')
