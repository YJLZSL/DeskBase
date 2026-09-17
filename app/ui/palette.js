/* ============================================================
   DeskBase 命令面板（覆盖式，Ctrl/Cmd+K）
   ============================================================
   对外只暴露 window.DeskBasePalette = { register, open, close, isOpen }。

   为什么是一个自包含的文件：面板要在任何页面状态下都能唤起（docs 的
   P5-04 要求「Ctrl+K 在任意位置可唤起」），所以它不能依赖 app.js 的内部
   变量，也不许往 index.html 里塞标记 —— DOM 全部在这里运行时创建，
   挂在 body 末尾。这样即使别的模块还没加载，面板也是完整的。

   ## 中文用户的关键点：拼音首字母

   目标用户是小老板、会计、仓管、跟单员。让他们用键盘敲「新建笔记」四个字
   不现实（要么切输入法、要么记快捷键），所以搜索必须支持 **xjbj**。

   ## 已知不足（写在最前面，免得被当成 bug）

   1. 拼音只到「首字母」，不提供全拼。见表里 PY_GROUPS 的说明。
   2. 多音字按 GB2312 的主读音归属（长→c、重→z、行→x）。要按别的读音搜，
      注册命令时自己给 py 字段（如 py: "chong"）。
   3. 匹配结果不做字符高亮：拼音命中的位置在原文里对不上（x→新 这种映射
      在首字母串里才存在），硬高亮会指错地方，所以干脆不高亮。
   ============================================================ */
(function () {
  "use strict";

  // ============================================================
  // 一、常用汉字拼音首字母表
  // ============================================================
  /* 为什么是「按首字母分组的字符串」而不是一张 字→字母 的映射表：
     映射表要写 3755 行，而分组字符串只占约 11 KB，还顺便能一眼看出覆盖范围。

     数据来源与生成方式（离线做的，不进仓库）：
       GB2312 **一级汉字**（区 16–55，编码 0xB0A1–0xD7F9，共 3755 字）本身
       就是**按拼音排序**的。于是只要知道 23 个字母各自的起始锚点字，就能把
       每个字归到它的首字母下。锚点字是这批：
         啊 芭 擦 搭 蛾 发 噶 哈 击 喀 垃 妈 拿 哦 啪 期 然 撒 塌 挖 昔 压 匝
       用 .NET 的 GB2312 编码离线算出每个锚点的编码，并逐条校验过：
         · 锚点编码严格递增；
         · 每个锚点前一个字都属于上一组（澳→芭、怖→擦、错→搭 …… 孕→匝），
           23 条全对 —— 说明字母边界卡在正确的位置上；
         · 另外抽检了 123 个常用字（含多音字）与 23 个组首字，全部一致。

     局限（很重要，别指望它做全拼）：
       · 只覆盖上面的 3755 个常用字。生僻字、繁体字、异体字（裡、貳…）不在
         表里 —— 它们不会被拼音命中，但**仍能被原文（汉字子序列）命中**。
       · 一个字的归属是它在该编码区里的主读音，多音字只按这一个读音算。
       · 想要全拼，注册时给命令加 py 字段（见 register 的注释）。 */
  const PY_GROUPS = {
    a: "啊阿埃挨哎唉哀皑癌蔼矮艾碍爱隘鞍氨安俺按暗岸胺案肮昂盎凹敖熬翱袄傲奥懊澳",
    b: "芭捌扒叭吧笆八疤巴拔跋靶把耙坝霸罢爸白柏百摆佰败拜稗斑班搬扳般颁板版扮拌伴瓣半办绊邦帮梆榜膀绑棒磅蚌镑傍谤苞胞包褒剥薄雹保堡饱宝抱报暴豹鲍爆杯碑悲卑北辈背贝钡倍狈备惫焙被奔苯本笨崩绷甭泵蹦迸逼鼻比鄙笔彼碧蓖蔽毕毙毖币庇痹闭敝弊必辟壁臂避陛鞭边编贬扁便变卞辨辩辫遍标彪膘表鳖憋别瘪彬斌濒滨宾摈兵冰柄丙秉饼炳病并玻菠播拨钵波博勃搏铂箔伯帛舶脖膊渤泊驳捕卜哺补埠不布步簿部怖",
    c: "擦猜裁材才财睬踩采彩菜蔡餐参蚕残惭惨灿苍舱仓沧藏操糙槽曹草厕策侧册测层蹭插叉茬茶查碴搽察岔差诧拆柴豺搀掺蝉馋谗缠铲产阐颤昌猖场尝常长偿肠厂敞畅唱倡超抄钞朝嘲潮巢吵炒车扯撤掣彻澈郴臣辰尘晨忱沉陈趁衬撑称城橙成呈乘程惩澄诚承逞骋秤吃痴持匙池迟弛驰耻齿侈尺赤翅斥炽充冲虫崇宠抽酬畴踌稠愁筹仇绸瞅丑臭初出橱厨躇锄雏滁除楚础储矗搐触处揣川穿椽传船喘串疮窗幢床闯创吹炊捶锤垂春椿醇唇淳纯蠢戳绰疵茨磁雌辞慈瓷词此刺赐次聪葱囱匆从丛凑粗醋簇促蹿篡窜摧崔催脆瘁粹淬翠村存寸磋撮搓措挫错",
    d: "搭达答瘩打大呆歹傣戴带殆代贷袋待逮怠耽担丹单郸掸胆旦氮但惮淡诞弹蛋当挡党荡档刀捣蹈倒岛祷导到稻悼道盗德得的蹬灯登等瞪凳邓堤低滴迪敌笛狄涤翟嫡抵底地蒂第帝弟递缔颠掂滇碘点典靛垫电佃甸店惦奠淀殿碉叼雕凋刁掉吊钓调跌爹碟蝶迭谍叠丁盯叮钉顶鼎锭定订丢东冬董懂动栋侗恫冻洞兜抖斗陡豆逗痘都督毒犊独读堵睹赌杜镀肚度渡妒端短锻段断缎堆兑队对墩吨蹲敦顿囤钝盾遁掇哆多夺垛躲朵跺舵剁惰堕",
    e: "蛾峨鹅俄额讹娥恶厄扼遏鄂饿恩而儿耳尔饵洱二贰",
    f: "发罚筏伐乏阀法珐藩帆番翻樊矾钒繁凡烦反返范贩犯饭泛坊芳方肪房防妨仿访纺放菲非啡飞肥匪诽吠肺废沸费芬酚吩氛分纷坟焚汾粉奋份忿愤粪丰封枫蜂峰锋风疯烽逢冯缝讽奉凤佛否夫敷肤孵扶拂辐幅氟符伏俘服浮涪福袱弗甫抚辅俯釜斧脯腑府腐赴副覆赋复傅付阜父腹负富讣附妇缚咐",
    g: "噶嘎该改概钙盖溉干甘杆柑竿肝赶感秆敢赣冈刚钢缸肛纲岗港杠篙皋高膏羔糕搞镐稿告哥歌搁戈鸽胳疙割革葛格蛤阁隔铬个各给根跟耕更庚羹埂耿梗工攻功恭龚供躬公宫弓巩汞拱贡共钩勾沟苟狗垢构购够辜菇咕箍估沽孤姑鼓古蛊骨谷股故顾固雇刮瓜剐寡挂褂乖拐怪棺关官冠观管馆罐惯灌贯光广逛瑰规圭硅归龟闺轨鬼诡癸桂柜跪贵刽辊滚棍锅郭国果裹过",
    h: "哈骸孩海氦亥害骇酣憨邯韩含涵寒函喊罕翰撼捍旱憾悍焊汗汉夯杭航壕嚎豪毫郝好耗号浩呵喝荷菏核禾和何合盒貉阂河涸赫褐鹤贺嘿黑痕很狠恨哼亨横衡恒轰哄烘虹鸿洪宏弘红喉侯猴吼厚候后呼乎忽瑚壶葫胡蝴狐糊湖弧虎唬护互沪户花哗华猾滑画划化话槐徊怀淮坏欢环桓还缓换患唤痪豢焕涣宦幻荒慌黄磺蝗簧皇凰惶煌晃幌恍谎灰挥辉徽恢蛔回毁悔慧卉惠晦贿秽会烩汇讳诲绘荤昏婚魂浑混豁活伙火获或惑霍货祸",
    j: "击圾基机畸稽积箕肌饥迹激讥鸡姬绩缉吉极棘辑籍集及急疾汲即嫉级挤几脊己蓟技冀季伎祭剂悸济寄寂计记既忌际妓继纪嘉枷夹佳家加荚颊贾甲钾假稼价架驾嫁歼监坚尖笺间煎兼肩艰奸缄茧检柬碱硷拣捡简俭剪减荐槛鉴践贱见键箭件健舰剑饯渐溅涧建僵姜将浆江疆蒋桨奖讲匠酱降蕉椒礁焦胶交郊浇骄娇嚼搅铰矫侥脚狡角饺缴绞剿教酵轿较叫窖揭接皆秸街阶截劫节桔杰捷睫竭洁结解姐戒藉芥界借介疥诫届巾筋斤金今津襟紧锦仅谨进靳晋禁近烬浸尽劲荆兢茎睛晶鲸京惊精粳经井警景颈静境敬镜径痉靖竟竞净炯窘揪究纠玖韭久灸九酒厩救旧臼舅咎就疚鞠拘狙疽居驹菊局咀矩举沮聚拒据巨具距踞锯俱句惧炬剧捐鹃娟倦眷卷绢撅攫抉掘倔爵觉决诀绝均菌钧军君峻俊竣浚郡骏",
    k: "喀咖卡咯开揩楷凯慨刊堪勘坎砍看康慷糠扛抗亢炕考拷烤靠坷苛柯棵磕颗科壳咳可渴克刻客课肯啃垦恳坑吭空恐孔控抠口扣寇枯哭窟苦酷库裤夸垮挎跨胯块筷侩快宽款匡筐狂框矿眶旷况亏盔岿窥葵奎魁傀馈愧溃坤昆捆困括扩廓阔",
    l: "垃拉喇蜡腊辣啦莱来赖蓝婪栏拦篮阑兰澜谰揽览懒缆烂滥琅榔狼廊郎朗浪捞劳牢老佬姥酪烙涝勒乐雷镭蕾磊累儡垒擂肋类泪棱楞冷厘梨犁黎篱狸离漓理李里鲤礼莉荔吏栗丽厉励砾历利傈例俐痢立粒沥隶力璃哩俩联莲连镰廉怜涟帘敛脸链恋炼练粮凉梁粱良两辆量晾亮谅撩聊僚疗燎寥辽潦了撂镣廖料列裂烈劣猎琳林磷霖临邻鳞淋凛赁吝拎玲菱零龄铃伶羚凌灵陵岭领另令溜琉榴硫馏留刘瘤流柳六龙聋咙笼窿隆垄拢陇楼娄搂篓漏陋芦卢颅庐炉掳卤虏鲁麓碌露路赂鹿潞禄录陆戮驴吕铝侣旅履屡缕虑氯律率滤绿峦挛孪滦卵乱掠略抡轮伦仑沦纶论萝螺罗逻锣箩骡裸落洛骆络",
    m: "妈麻玛码蚂马骂嘛吗埋买麦卖迈脉瞒馒蛮满蔓曼慢漫谩芒茫盲氓忙莽猫茅锚毛矛铆卯茂冒帽貌贸么玫枚梅酶霉煤没眉媒镁每美昧寐妹媚门闷们萌蒙檬盟锰猛梦孟眯醚靡糜迷谜弥米秘觅泌蜜密幂棉眠绵冕免勉娩缅面苗描瞄藐秒渺庙妙蔑灭民抿皿敏悯闽明螟鸣铭名命谬摸摹蘑模膜磨摩魔抹末莫墨默沫漠寞陌谋牟某拇牡亩姆母墓暮幕募慕木目睦牧穆",
    n: "拿哪呐钠那娜纳氖乃奶耐奈南男难囊挠脑恼闹淖呢馁内嫩能妮霓倪泥尼拟你匿腻逆溺蔫拈年碾撵捻念娘酿鸟尿捏聂孽啮镊镍涅您柠狞凝宁拧泞牛扭钮纽脓浓农弄奴努怒女暖虐疟挪懦糯诺",
    o: "哦欧鸥殴藕呕偶沤",
    p: "啪趴爬帕怕琶拍排牌徘湃派攀潘盘磐盼畔判叛乓庞旁耪胖抛咆刨炮袍跑泡呸胚培裴赔陪配佩沛喷盆砰抨烹澎彭蓬棚硼篷膨朋鹏捧碰坯砒霹批披劈琵毗啤脾疲皮匹痞僻屁譬篇偏片骗飘漂瓢票撇瞥拼频贫品聘乒坪苹萍平凭瓶评屏坡泼颇婆破魄迫粕剖扑铺仆莆葡菩蒲埔朴圃普浦谱曝瀑",
    q: "期欺栖戚妻七凄漆柒沏其棋奇歧畦崎脐齐旗祈祁骑起岂乞企启契砌器气迄弃汽泣讫掐恰洽牵扦钎铅千迁签仟谦乾黔钱钳前潜遣浅谴堑嵌欠歉枪呛腔羌墙蔷强抢橇锹敲悄桥瞧乔侨巧鞘撬翘峭俏窍切茄且怯窃钦侵亲秦琴勤芹擒禽寝沁青轻氢倾卿清擎晴氰情顷请庆琼穷秋丘邱球求囚酋泅趋区蛆曲躯屈驱渠取娶龋趣去圈颧权醛泉全痊拳犬券劝缺炔瘸却鹊榷确雀裙群",
    r: "然燃冉染瓤壤攘嚷让饶扰绕惹热壬仁人忍韧任认刃妊纫扔仍日戎茸蓉荣融熔溶容绒冗揉柔肉茹蠕儒孺如辱乳汝入褥软阮蕊瑞锐闰润若弱",
    s: "撒洒萨腮鳃塞赛三叁伞散桑嗓丧搔骚扫嫂瑟色涩森僧莎砂杀刹沙纱傻啥煞筛晒珊苫杉山删煽衫闪陕擅赡膳善汕扇缮墒伤商赏晌上尚裳梢捎稍烧芍勺韶少哨邵绍奢赊蛇舌舍赦摄射慑涉社设砷申呻伸身深娠绅神沈审婶甚肾慎渗声生甥牲升绳省盛剩胜圣师失狮施湿诗尸虱十石拾时什食蚀实识史矢使屎驶始式示士世柿事拭誓逝势是嗜噬适仕侍释饰氏市恃室视试收手首守寿授售受瘦兽蔬枢梳殊抒输叔舒淑疏书赎孰熟薯暑曙署蜀黍鼠属术述树束戍竖墅庶数漱恕刷耍摔衰甩帅栓拴霜双爽谁水睡税吮瞬顺舜说硕朔烁斯撕嘶思私司丝死肆寺嗣四伺似饲巳松耸怂颂送宋讼诵搜艘擞嗽苏酥俗素速粟僳塑溯宿诉肃酸蒜算虽隋随绥髓碎岁穗遂隧祟孙损笋蓑梭唆缩琐索锁所",
    t: "塌他它她塔獭挞蹋踏胎苔抬台泰酞太态汰坍摊贪瘫滩坛檀痰潭谭谈坦毯袒碳探叹炭汤塘搪堂棠膛唐糖倘躺淌趟烫掏涛滔绦萄桃逃淘陶讨套特藤腾疼誊梯剔踢锑提题蹄啼体替嚏惕涕剃屉天添填田甜恬舔腆挑条迢眺跳贴铁帖厅听烃汀廷停亭庭挺艇通桐酮瞳同铜彤童桶捅筒统痛偷投头透凸秃突图徒途涂屠土吐兔湍团推颓腿蜕褪退吞屯臀拖托脱鸵陀驮驼椭妥拓唾",
    w: "挖哇蛙洼娃瓦袜歪外豌弯湾玩顽丸烷完碗挽晚皖惋宛婉万腕汪王亡枉网往旺望忘妄威巍微危韦违桅围唯惟为潍维苇萎委伟伪尾纬未蔚味畏胃喂魏位渭谓尉慰卫瘟温蚊文闻纹吻稳紊问嗡翁瓮挝蜗涡窝我斡卧握沃巫呜钨乌污诬屋无芜梧吾吴毋武五捂午舞伍侮坞戊雾晤物勿务悟误",
    x: "昔熙析西硒矽晰嘻吸锡牺稀息希悉膝夕惜熄烯溪汐犀檄袭席习媳喜铣洗系隙戏细瞎虾匣霞辖暇峡侠狭下厦夏吓掀锨先仙鲜纤咸贤衔舷闲涎弦嫌显险现献县腺馅羡宪陷限线相厢镶香箱襄湘乡翔祥详想响享项巷橡像向象萧硝霄削哮嚣销消宵淆晓小孝校肖啸笑效楔些歇蝎鞋协挟携邪斜胁谐写械卸蟹懈泄泻谢屑薪芯锌欣辛新忻心信衅星腥猩惺兴刑型形邢行醒幸杏性姓兄凶胸匈汹雄熊休修羞朽嗅锈秀袖绣墟戌需虚嘘须徐许蓄酗叙旭序畜恤絮婿绪续轩喧宣悬旋玄选癣眩绚靴薛学穴雪血勋熏循旬询寻驯巡殉汛训讯逊迅",
    y: "压押鸦鸭呀丫芽牙蚜崖衙涯雅哑亚讶焉咽阉烟淹盐严研蜒岩延言颜阎炎沿奄掩眼衍演艳堰燕厌砚雁唁彦焰宴谚验殃央鸯秧杨扬佯疡羊洋阳氧仰痒养样漾邀腰妖瑶摇尧遥窑谣姚咬舀药要耀椰噎耶爷野冶也页掖业叶曳腋夜液一壹医揖铱依伊衣颐夷遗移仪胰疑沂宜姨彝椅蚁倚已乙矣以艺抑易邑屹亿役臆逸肄疫亦裔意毅忆义益溢诣议谊译异翼翌绎茵荫因殷音阴姻吟银淫寅饮尹引隐印英樱婴鹰应缨莹萤营荧蝇迎赢盈影颖硬映哟拥佣臃痈庸雍踊蛹咏泳涌永恿勇用幽优悠忧尤由邮铀犹油游酉有友右佑釉诱又幼迂淤于盂榆虞愚舆余俞逾鱼愉渝渔隅予娱雨与屿禹宇语羽玉域芋郁吁遇喻峪御愈欲狱育誉浴寓裕预豫驭鸳渊冤元垣袁原援辕园员圆猿源缘远苑愿怨院曰约越跃钥岳粤月悦阅耘云郧匀陨允运蕴酝晕韵孕",
    z: "匝砸杂栽哉灾宰载再在咱攒暂赞赃脏葬遭糟凿藻枣早澡蚤躁噪造皂灶燥责择则泽贼怎增憎曾赠扎喳渣札轧铡闸眨栅榨咋乍炸诈摘斋宅窄债寨瞻毡詹粘沾盏斩辗崭展蘸栈占战站湛绽樟章彰漳张掌涨杖丈帐账仗胀瘴障招昭找沼赵照罩兆肇召遮折哲蛰辙者锗蔗这浙珍斟真甄砧臻贞针侦枕疹诊震振镇阵蒸挣睁征狰争怔整拯正政帧症郑证芝枝支吱蜘知肢脂汁之织职直植殖执值侄址指止趾只旨纸志挚掷至致置帜峙制智秩稚质炙痔滞治窒中盅忠钟衷终种肿重仲众舟周州洲诌粥轴肘帚咒皱宙昼骤珠株蛛朱猪诸诛逐竹烛煮拄瞩嘱主著柱助蛀贮铸筑住注祝驻抓爪拽专砖转撰赚篆桩庄装妆撞壮状椎锥追赘坠缀谆准捉拙卓桌琢茁酌啄着灼浊兹咨资姿滋淄孜紫仔籽滓子自渍字鬃棕踪宗综总纵邹走奏揍租足卒族祖诅阻组钻纂嘴醉最罪尊遵昨左佐柞做作坐座"
  };

  /** 字 → 首字母。首次用到才建表（3755 条映射没必要占用启动时间）。 */
  let pyMap = null;
  function pyOf(ch) {
    if (!pyMap) {
      pyMap = new Map();
      for (const letter of Object.keys(PY_GROUPS)) {
        const group = PY_GROUPS[letter];
        for (let i = 0; i < group.length; i++) pyMap.set(group[i], letter);
      }
    }
    return pyMap.get(ch);
  }

  /** 标题 → 首字母串：「新建笔记」→「xjbj」。
   *  表里没有的字、以及标点空格一律跳过；英文数字原样保留（小写）——
   *  这样「导出 Excel」既能被 dc 命中，也能被 excel 命中。 */
  function initialsOf(text) {
    let out = "";
    for (const ch of text) {
      const py = pyOf(ch);
      if (py) out += py;
      else if (ch >= "a" && ch <= "z") out += ch;
      else if (ch >= "0" && ch <= "9") out += ch;
    }
    return out;
  }

  // ============================================================
  // 二、匹配
  // ============================================================

  /**
   * 子序列匹配 + 打分：q 的每个字符要按顺序出现在 text 里（中间可以跳过）。
   * 返回 -1 表示不匹配。分数只用来排序，绝对值无意义。
   *
   * 打分遵循两条直觉：
   *   连续命中比散落命中值钱（"biji" 命中 "biji" 应该赢过 "b...i...j...i"）
   *   命中位置越靠前越值钱（前缀命中是最好的）
   */
  function fuzzyScore(q, text) {
    if (!text) return -1;
    let ti = 0;
    let qi = 0;
    let score = 0;
    let run = 0;
    let first = -1;
    while (ti < text.length && qi < q.length) {
      if (text.charCodeAt(ti) === q.charCodeAt(qi)) {
        if (first < 0) first = ti;
        run++;
        score += 10 + (run - 1) * 6;
        if (ti === 0) score += 12;
        qi++;
      } else {
        run = 0;
      }
      ti++;
    }
    if (qi < q.length) return -1;
    score -= first * 2;
    score -= Math.max(0, text.length - q.length) * 0.1;
    return score;
  }

  /* 派生串（原文 / 首字母 / 调用方给的拼音别名）用 WeakMap 缓存：
     每次按键都要把全部命令重扫一遍，字符串不能重复算；用 WeakMap 是为了
     不往调用方的命令对象上挂字段。 */
  const targets = new WeakMap();
  function targetsOf(cmd) {
    let t = targets.get(cmd);
    if (!t) {
      const raw = cmd.title.toLowerCase();
      t = {
        raw: raw,
        ini: initialsOf(raw),
        extra: cmd.py ? cmd.py.toLowerCase().replace(/\s+/g, "") : "",
      };
      targets.set(cmd, t);
    }
    return t;
  }

  /** 三条候选串取最高分。权重让「原文命中」排在「拼音命中」前面：
   *  用户能敲出原文的字符，说明这条更确定。 */
  function scoreOf(cmd, q) {
    const t = targetsOf(cmd);
    let best = -1;
    const a = fuzzyScore(q, t.raw);
    if (a >= 0) best = a;
    const b = fuzzyScore(q, t.ini);
    if (b >= 0) best = Math.max(best, b * 0.92);
    if (t.extra) {
      const c = fuzzyScore(q, t.extra);
      if (c >= 0) best = Math.max(best, c * 0.85);
    }
    return best;
  }

  // ============================================================
  // 三、最近使用（localStorage）
  // ============================================================
  const KEY_RECENT = "deskbase.palette.recent";
  const RECENT_SHOW = 5;    // 空输入时显示几条
  const RECENT_KEEP = 20;   // 最多记几条

  function readRecent() {
    try {
      const arr = JSON.parse(localStorage.getItem(KEY_RECENT) || "[]");
      return Array.isArray(arr) ? arr.filter((x) => typeof x === "string") : [];
    } catch (e) {
      return [];   // 存储被禁用/被写坏时静默降级：没有「最近」不影响搜索
    }
  }
  function rememberRecent(id) {
    const arr = readRecent().filter((x) => x !== id);
    arr.unshift(id);
    try {
      localStorage.setItem(KEY_RECENT, JSON.stringify(arr.slice(0, RECENT_KEEP)));
    } catch (e) {}
  }

  // ============================================================
  // 四、状态与常量
  // ============================================================
  const MAX_ROWS = 50;      // 渲染上限。再多也不看，只是把首屏拖慢
  const MAX_STAGGER = 8;    // 错峰只累加到第 8 项（18ms × 8 = 144ms 封顶）
  const WARN_AT = 500;      // 注册量超过这个数就在控制台提醒一次
  const TYPE_DELAY = 120;   // 输入防抖：≤150ms（docs 性能预算里的一条）

  const state = {
    open: false,
    commands: [],
    byId: new Map(),
    warned: false,
    query: "",
    rows: [],
    shown: [],           // 当前渲染出来的命令（按行序），回车时按下标取
    sel: -1,
    total: 0,
    truncated: 0,
    recentIds: [],
    stagger: false,      // 只有刚打开那一次做逐项错峰动画
    lastFocus: null,     // 打开前的焦点，关闭时还回去
    timer: 0,
  };

  let dom = null;

  // ============================================================
  // 五、DOM（全部运行时创建）
  // ============================================================
  const SVG_NS = "http://www.w3.org/2000/svg";

  /** 内联一个放大镜 —— 不引用 index.html 里的 sprite：
   *  面板要能独立立起来（测试页、以后挪到别的页面都不该缺图标）。 */
  function searchIcon() {
    const svg = document.createElementNS(SVG_NS, "svg");
    svg.setAttribute("class", "ico");
    svg.setAttribute("viewBox", "0 0 20 20");
    svg.setAttribute("aria-hidden", "true");
    const c = document.createElementNS(SVG_NS, "circle");
    c.setAttribute("cx", "9");
    c.setAttribute("cy", "9");
    c.setAttribute("r", "5.2");
    const p = document.createElementNS(SVG_NS, "path");
    p.setAttribute("d", "M12.9 12.9 17 17");
    svg.appendChild(c);
    svg.appendChild(p);
    return svg;
  }

  function kbd(text) {
    const s = document.createElement("span");
    s.className = "dp-kbd";
    s.textContent = text;
    return s;
  }

  function buildDom() {
    if (dom) return dom;

    const root = document.createElement("div");
    root.className = "dp-root";
    root.dataset.open = "false";

    const scrim = document.createElement("div");
    scrim.className = "dp-scrim";
    scrim.setAttribute("aria-hidden", "true");

    const panel = document.createElement("div");
    panel.className = "dp-panel";
    panel.setAttribute("role", "dialog");
    panel.setAttribute("aria-modal", "true");
    panel.setAttribute("aria-label", "命令面板");

    const head = document.createElement("div");
    head.className = "dp-head";
    head.appendChild(searchIcon());

    const input = document.createElement("input");
    input.className = "dp-input";
    input.type = "text";           // 不用 type=search：那个自带的清除按钮在浮层里多余
    input.placeholder = "搜索命令…（支持拼音首字母，如 xjbj）";
    input.autocomplete = "off";
    input.spellcheck = false;
    input.setAttribute("role", "combobox");
    input.setAttribute("aria-expanded", "true");
    input.setAttribute("aria-controls", "dp-list");
    input.setAttribute("aria-autocomplete", "list");
    head.appendChild(input);
    head.appendChild(kbd("Esc"));

    const body = document.createElement("div");
    body.className = "dp-body";
    body.id = "dp-list";
    body.setAttribute("role", "listbox");
    body.setAttribute("aria-label", "命令列表");
    const hi = document.createElement("div");
    hi.className = "dp-hi";        // 滑动高亮：一块背景，只动 transform
    hi.setAttribute("aria-hidden", "true");
    body.appendChild(hi);

    const foot = document.createElement("div");
    foot.className = "dp-foot";
    const count = document.createElement("span");
    count.className = "dp-count";
    foot.appendChild(count);
    const hint = document.createElement("span");
    hint.className = "dp-hint";
    hint.appendChild(kbd("↑↓"));
    hint.appendChild(document.createTextNode(" 选择 · "));
    hint.appendChild(kbd("Enter"));
    hint.appendChild(document.createTextNode(" 执行"));
    foot.appendChild(hint);

    panel.appendChild(head);
    panel.appendChild(body);
    panel.appendChild(foot);
    root.appendChild(scrim);
    root.appendChild(panel);
    document.body.appendChild(root);

    // 点面板外面 = 关闭。用 mousedown 而不是 click：手指/鼠标落下就该有反应。
    // 遮罩在淡出的 140ms 里仍然占着位置，所以这次点击不会"穿透"到底下的界面。
    root.addEventListener("mousedown", (e) => {
      if (!panel.contains(e.target)) close();
    });

    input.addEventListener("input", onInput);
    input.addEventListener("keydown", onInputKey);
    // 输入法组字结束时补一次渲染：组字过程中的 input 事件被 isComposing 挡掉了，
    // 最后一次 input 如果也带着 isComposing=true，就得靠这里把结果算出来
    input.addEventListener("compositionend", schedule);

    dom = { root: root, scrim: scrim, panel: panel, input: input, body: body, hi: hi, count: count };
    return dom;
  }

  // ============================================================
  // 六、渲染
  // ============================================================
  function schedule() {
    clearTimeout(state.timer);
    state.timer = setTimeout(() => {
      state.timer = 0;
      render();
    }, TYPE_DELAY);
  }

  /** 有未落地的防抖就立刻落地。Enter 之前必须调它 ——
   *  否则"打完字马上按回车"会执行上一帧结果里的那条命令（真会出事）。 */
  function flush() {
    if (!state.timer) return;
    clearTimeout(state.timer);
    state.timer = 0;
    render();
  }

  function onInput(e) {
    if (!dom) return;
    state.query = dom.input.value;
    // 输入法正在组字时，输入框里的是拼音草稿（"xinjian"），不是用户想搜的词。
    // 这时候渲染结果会乱跳，等 compositionend 再算。
    if (e && e.isComposing) return;
    schedule();
  }

  function computeItems() {
    const q = state.query.trim().toLowerCase().replace(/\s+/g, "");
    if (!q) {
      const items = [];
      for (const id of state.recentIds) {
        const cmd = state.byId.get(id);
        if (cmd) items.push({ cmd: cmd, score: 0, order: items.length });
        if (items.length >= RECENT_SHOW) break;
      }
      return { items: items, recent: true };
    }
    const scored = [];
    for (let i = 0; i < state.commands.length; i++) {
      const cmd = state.commands[i];
      const s = scoreOf(cmd, q);
      if (s < 0) continue;
      // 最近用过的稍微往前提：同一批候选里，刚用过的那条更可能是这次要找的
      scored.push({ cmd: cmd, score: s + (state.recentIds.indexOf(cmd.id) >= 0 ? 4 : 0), order: i });
    }
    scored.sort((a, b) => b.score - a.score || a.order - b.order);
    return { items: scored, recent: false };
  }

  function render() {
    if (!dom) return;
    const res = computeItems();
    const shown = res.items.slice(0, MAX_ROWS);
    state.total = res.items.length;
    state.truncated = state.total - shown.length;
    state.shown = shown.map((it) => it.cmd);

    // 只清掉上一次的行与组标题，**不能**用 textContent = ""：
    // 那会把滑动高亮（.dp-hi）一起删掉，而 dom.hi 还留着引用 ——
    // 之后所有"移动高亮"的操作都作用在一个已经摘下来的节点上，看不见任何效果。
    for (const child of Array.from(dom.body.children)) {
      if (child !== dom.hi) child.remove();
    }
    dom.body.dataset.anim = state.stagger ? "on" : "off";
    dom.body.dataset.hi = "off";    // 重建期间先收起高亮，免得它停在错位置
    state.rows = [];
    state.sel = -1;

    if (!shown.length) {
      const empty = document.createElement("div");
      empty.className = "dp-empty";
      empty.textContent = res.recent
        ? "还没有用过命令。输入关键词试试 —— 支持拼音首字母（xjbj = 新建笔记）。"
        : "没找到「" + state.query.trim() + "」。可以只打一部分，或用拼音首字母（如 xbj）。";
      dom.body.appendChild(empty);
      updateFoot(res.recent);
      state.stagger = false;
      return;
    }

    // 分组：组的先后 = 组内最好那条的排名，所以最相关的一组在最上面。
    // 组内保持得分顺序（不重排），用户看到的顺序才和"最可能想要"一致。
    // 空输入是特例：那 5 条来自不同模块，混着显示各自的分组名会碎成一片，
    // 统一收在「最近使用」下更像一段"刚做过的事"。
    const order = [];
    const buckets = new Map();
    shown.forEach((it, i) => {
      const g = res.recent ? "最近使用" : it.cmd.group;
      if (!buckets.has(g)) {
        buckets.set(g, []);
        order.push(g);
      }
      buckets.get(g).push({ it: it, i: i });
    });

    for (const g of order) {
      const label = document.createElement("div");
      label.className = "dp-group-label";
      label.textContent = g;
      dom.body.appendChild(label);
      for (const entry of buckets.get(g)) {
        const row = buildRow(entry.it.cmd, entry.i);
        dom.body.appendChild(row);
        state.rows.push(row);
      }
    }

    updateFoot(res.recent);
    select(0, true);                // 默认选中第一条，但不要滑过去（瞬间就位）
    dom.body.scrollTop = 0;
    state.stagger = false;
  }

  function buildRow(cmd, index) {
    const row = document.createElement("button");
    row.type = "button";
    row.className = "dp-item";
    row.id = "dp-opt-" + index;
    row.setAttribute("role", "option");
    row.setAttribute("aria-selected", "false");
    row.tabIndex = -1;              // 焦点永远留在输入框里，行不进 Tab 序列
    // 逐项错峰的"第几项"。超过 MAX_STAGGER 就不再累加，否则列表长了要等很久
    row.style.setProperty("--i", String(Math.min(index, MAX_STAGGER)));

    const title = document.createElement("span");
    title.className = "t";
    title.textContent = cmd.title;
    row.appendChild(title);
    if (cmd.shortcut) row.appendChild(kbd(cmd.shortcut));

    // 鼠标移到哪，选中就滑到哪 —— 和键盘共用同一套"选中"语义，
    // 也免得出现"高亮在这条、回车执行那条"的错位
    row.addEventListener("mouseover", () => select(index));
    // 按下别让焦点离开输入框，否则用户还得再点回来才能继续打字
    row.addEventListener("mousedown", (e) => e.preventDefault());
    row.addEventListener("click", () => run(cmd));
    return row;
  }

  function updateFoot(recent) {
    if (!dom) return;
    // 类名保持 .dp-count 不变（选择器要稳定），状态用 data-kind 表达
    if (state.truncated > 0) {
      dom.count.dataset.kind = "more";
      dom.count.textContent = "还有 " + state.truncated + " 条，请继续输入";
      return;
    }
    delete dom.count.dataset.kind;
    if (!state.total) dom.count.textContent = "";
    else if (recent) dom.count.textContent = "最近使用 " + state.total + " 条";
    else dom.count.textContent = state.total + " 条结果";
  }

  // ============================================================
  // 七、选中与高亮
  // ============================================================
  /** 把高亮块滑到第 sel 行。snap=true 时不带过渡（列表重建/首次打开用，
   *  否则高亮会从上一帧的位置飘过来）。 */
  function placeHighlight(snap) {
    if (!dom) return;
    const row = state.rows[state.sel];
    if (!row) {
      dom.body.dataset.hi = "off";
      return;
    }
    if (snap) dom.hi.style.transition = "none";
    // offsetTop 与 CSS 里 .dp-hi 的 top:0 同以 .dp-body 的 padding box 为原点，
    // 所以行高、组标题高度怎么变都不用在这里改公式
    dom.hi.style.transform = "translateY(" + row.offsetTop + "px)";
    if (snap) {
      void dom.hi.offsetHeight;     // 强制结算这一帧，"无过渡"才真的生效
      dom.hi.style.transition = "";
    }
    dom.body.dataset.hi = "on";
  }

  function select(index, snap) {
    if (!dom) return;
    if (!state.rows.length) {
      state.sel = -1;
      placeHighlight(snap);
      return;
    }
    state.sel = Math.max(0, Math.min(index, state.rows.length - 1));
    for (let i = 0; i < state.rows.length; i++) {
      const row = state.rows[i];
      if (i === state.sel) row.setAttribute("aria-selected", "true");
      else row.removeAttribute("aria-selected");
    }
    dom.input.setAttribute("aria-activedescendant", state.rows[state.sel].id);
    placeHighlight(snap);
    // 键盘移动时把选中项带进视野；instant 滚动，别和滑动高亮打架
    if (!snap) state.rows[state.sel].scrollIntoView({ block: "nearest" });
  }

  function run(cmd) {
    if (!cmd) return;
    close();                        // 先关再执行：命令可能切页面/开对话框
    rememberRecent(cmd.id);
    state.recentIds = readRecent();
    try {
      const r = cmd.run();
      if (r && typeof r.then === "function") {
        r.catch((e) => console.error("[palette] 命令执行失败：" + cmd.id, e));
      }
    } catch (e) {
      // 一条命令炸了不该把面板带崩（虽然这时面板已经关了）
      console.error("[palette] 命令执行失败：" + cmd.id, e);
    }
  }

  function runSelected() {
    run(state.shown[state.sel]);
  }

  // ============================================================
  // 八、键盘
  // ============================================================
  function onInputKey(e) {
    // 输入法组字中：回车是"选候选字"，不是"执行命令"。不拦这一条，
    // 中文用户用拼音打字时按回车就会莫名执行一条命令。
    if (e.isComposing || e.keyCode === 229) return;

    const k = e.key;
    if (k === "Tab") {
      // Tab 不许把焦点带走：面板是模态的，焦点跑到底下的界面上就没法打字了
      e.preventDefault();
      return;
    }
    const handled =
      k === "ArrowDown" || k === "ArrowUp" || k === "Home" || k === "End" ||
      k === "PageDown" || k === "PageUp" || k === "Enter" || k === "Escape";
    if (!handled) return;
    e.preventDefault();

    if (k === "Escape") return close();
    if (k === "Enter") {
      flush();                      // 先把未落地的输入算完，再执行
      return runSelected();
    }
    if (k === "ArrowDown") return select(state.sel + 1);
    if (k === "ArrowUp") return select(state.sel - 1);
    if (k === "Home") return select(0);
    if (k === "End") return select(state.rows.length - 1);
    if (k === "PageDown") return select(state.sel + 8);
    if (k === "PageUp") return select(state.sel - 8);
  }

  /** 全局热键。用**捕获**阶段：app.js 里 Ctrl+K 现在是"聚焦顶栏搜索框"，
     两个都在 window 上，不抢在它前面拦下来就会同时触发（搜索框聚焦 +
     面板打开，焦点还会打架）。esc 同理：面板开着时它归面板。 */
  window.addEventListener(
    "keydown",
    function (e) {
      const key = e.key || "";
      if ((e.ctrlKey || e.metaKey) && !e.altKey && key.toLowerCase() === "k") {
        e.preventDefault();
        e.stopPropagation();
        if (state.open) close();
        else open("");
        return;
      }
      if (!state.open) return;
      if (key === "Escape") {
        e.preventDefault();
        e.stopPropagation();
        close();
        return;
      }
      if (key === "Tab" && dom && !dom.panel.contains(document.activeElement)) {
        // 焦点已经跑到面板外（比如用户在别处点了一下）：拽回输入框
        e.preventDefault();
        dom.input.focus();
      }
    },
    true
  );

  // ============================================================
  // 九、对外 API
  // ============================================================
  /**
   * 注册命令。可多次调用，后注册的追加在后面。
   *
   * 每条命令：
   *   { id, title, group, shortcut, run, py }
   *   id       必填，唯一。重复 id 只保留第一条（并警告）
   *   title    必填，显示名（会被拼音化）
   *   group    选填，分组小标题；缺省归入「其他」
   *   shortcut 选填，右侧显示，如 "Ctrl+N"。只显示、不接管键盘
   *   run      必填，函数；返回 Promise 时会接住异常
   *   py       选填，额外的拼音/别名串（如 "chong zhong"）。多音字、
   *            生僻字、全拼都靠它补 —— 面板自己不做全拼
   */
  function register(commands) {
    const list = Array.isArray(commands) ? commands : [commands];
    for (const raw of list) {
      if (!raw) continue;
      const id = String(raw.id || "");
      const title = String(raw.title || "");
      if (!id || !title || typeof raw.run !== "function") {
        console.warn("[palette] 命令缺 id/title/run，已跳过：", raw);
        continue;
      }
      if (state.byId.has(id)) {
        console.warn("[palette] 命令 id 重复，已忽略后一条：" + id);
        continue;
      }
      const cmd = {
        id: id,
        title: title,
        group: String(raw.group || "其他"),
        shortcut: String(raw.shortcut || ""),
        py: String(raw.py || raw.keywords || ""),
        run: raw.run,
      };
      state.commands.push(cmd);
      state.byId.set(id, cmd);
    }
    // 到 500 条只提醒一次：每次按键都要扫全表，这个量级还是毫秒级，
    // 但说明该考虑分类过滤/索引了 —— 提醒一次比每次刷屏有用
    if (!state.warned && state.commands.length > WARN_AT) {
      state.warned = true;
      console.warn(
        "[palette] 已注册 " + state.commands.length + " 条命令（> " + WARN_AT +
          "）：面板每次按键都会重扫全表并重排，到这个量级建议按模块拆分注册时机。"
      );
    }
    if (state.open) render();
  }

  function open(initialQuery) {
    buildDom();
    if (!dom.root.contains(document.activeElement)) {
      state.lastFocus = document.activeElement;   // 记下焦点，关闭时还回去
    }
    state.recentIds = readRecent();
    state.query = typeof initialQuery === "string" ? initialQuery : "";
    dom.input.value = state.query;
    state.stagger = true;          // 这一次重建做逐项错峰
    state.open = true;
    dom.root.dataset.open = "true";
    render();
    dom.input.focus();
    dom.input.select();            // 全选：用户可以直接覆盖输入
  }

  function close() {
    if (!state.open || !dom) return;
    state.open = false;
    clearTimeout(state.timer);
    state.timer = 0;
    dom.root.dataset.open = "false";   // 退出动画由 CSS 的 allow-discrete 负责演完
    dom.body.dataset.hi = "off";
    // 焦点归还：只有焦点还在面板里时才抢回来（用户可能已经点到别处了）
    if (dom.root.contains(document.activeElement) &&
        state.lastFocus && document.contains(state.lastFocus) &&
        typeof state.lastFocus.focus === "function") {
      state.lastFocus.focus();
    }
  }

  function isOpen() {
    return state.open;
  }

  window.DeskBasePalette = {
    register: register,
    open: open,
    close: close,
    isOpen: isOpen,
  };
})();
